//! Functions for identifying which nodes should be materialized, and what indices should be used
//! for those materializations.
//!
//! This module also holds the logic for *identifying* state that must be transferred from other
//! domains, but does not perform that copying itself (that is the role of the `augmentation`
//! module).

use std::collections::{HashMap, HashSet};
use std::fmt::{self, Display};

use bimap::BiHashMap;
use dataflow::prelude::*;
use dataflow::{DomainRequest, LookupIndex};
use petgraph::graph::NodeIndex;
use readyset_errors::{internal, internal_err, invariant, ReadySetError, ReadySetResult};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info_span, trace};

use crate::controller::keys::{self, RawReplayPath};
use crate::controller::migrate::DomainMigrationPlan;
use crate::controller::state::Graphviz;

mod plan;

type Indices = HashSet<Index>;

pub(crate) struct InvalidEdge {
    pub parent: NodeIndex,
    pub child: NodeIndex,
}

/// Strategy for determining which (partial) materializations should be placed beyond the
/// materialization frontier.
///
/// Note that no matter what this is set to, all nodes whose name starts with `SHALLOW_` will be
/// placed beyond the frontier.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, clap::ValueEnum, Default)]
pub enum FrontierStrategy {
    /// Place no nodes beyond the frontier (this is the default).
    #[default]
    None,
    /// Place all partial materializations beyond the frontier.
    AllPartial,
    /// Place all partial readers beyond the frontier.
    Readers,
}

impl Display for FrontierStrategy {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::None => write!(f, "none"),
            Self::AllPartial => write!(f, "all-partial"),
            Self::Readers => write!(f, "readers"),
        }
    }
}

#[derive(Debug)]
enum IndexObligation {
    /// An obligation to index a particular set of columns with a particular index type in a node.
    ///
    /// A lookup obligation can be created either if a node asks for its own state to be
    /// materialized, or if a node indicates that it will perform lookups on its ancestors
    Lookup(LookupIndex),

    /// An obligation to index a particular set of columns for replays into a node
    ///
    /// Replay indexes are special, in that they can be hoisted past *all* nodes, including across
    /// domain boundaries. They are also special in that they also need to be carried along all the
    /// way to the nearest *full* materialization.
    Replay(Index),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    /// Whether the creation of [`PacketFilter`]s for egresses before readers is enabled.
    ///
    /// Defaults to false
    ///
    /// [`PacketFilter`]: readyset_dataflow::node::special::PacketFilter
    pub packet_filters_enabled: bool,

    /// Whether queries that require full materialization are allowed.
    ///
    /// If this is set to false, migrations that add queries that require full materialization will
    /// return [`ReadySetError::Unsupported`].
    ///
    /// Defaults to `false`
    pub allow_full_materialization: bool,

    /// Whether queries that contain straddled joins (joins with partial keys traced to both
    /// parent) are allowed
    ///
    /// If this is set to false, migrations that add queries that include straddled joins will
    /// return [`ReadySetError::Unsupported`].
    ///
    /// Defaults to `false`
    #[serde(default)]
    pub allow_straddled_joins: bool,

    /// Strategy for determining which (partial) materializations should be placed beyond the
    /// materialization frontier.
    ///
    /// Defaults to [`FrontierStrategy::None`]
    pub frontier_strategy: FrontierStrategy,

    /// Whether partial node creation is enabled at all.
    ///
    /// Defaults to true.
    pub partial_enabled: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            packet_filters_enabled: false,
            allow_full_materialization: false,
            allow_straddled_joins: false,
            partial_enabled: true,
            frontier_strategy: FrontierStrategy::None,
        }
    }
}

/// Struct containing (authoritative!) information about which nodes in a graph are materialized
/// (store their output state either in-memory or on-disk), and in what way those materializations
/// are indexed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(in crate::controller) struct Materializations {
    /// Nodes that are (fully or partially) materialized.
    // Skipping this field as we will rebuild the [`Materializations`] state
    // upon recovery.
    #[serde(skip)]
    have: HashMap<NodeIndex, Indices>,
    /// Nodes that *were* (fully or partially) as of the last time we called [`commit`].
    ///
    /// Used to validate that we're not adding any materializations we shouldn't (eg newly
    /// materialized nodes with materialized children)
    ///
    /// [`extend`]: Materializations::extend
    /// [`commit`]: Materializations::commit
    #[serde(skip)]
    had: HashSet<NodeIndex>,
    /// Nodes materialized since the last time `commit()` was invoked.
    #[serde(skip)]
    added: HashMap<NodeIndex, Indices>,

    /// Weak indices added since the last time `commit()` was invoked
    #[serde(skip)]
    added_weak: HashMap<NodeIndex, Indices>,

    /// Readers added since the last time `commit()` was invoked.
    #[serde(skip)]
    new_readers: HashSet<NodeIndex>,

    /// A list of replay paths for each node, indexed by tag.
    #[serde(with = "serde_with::rust::hashmap_as_tuple_list")]
    pub(in crate::controller) paths: HashMap<NodeIndex, BiHashMap<Tag, (Index, Vec<NodeIndex>)>>,

    /// Map of full nodes that are duplicates of partial nodes. Entries are added when we perform
    /// rerouting of full nodes found below partial nodes in migration planning.
    #[serde(with = "serde_with::rust::hashmap_as_tuple_list")]
    pub(in crate::controller) redundant_partial: HashMap<NodeIndex, NodeIndex>,

    // Skipping this field as we will rebuild the [`Materializations`] state
    // upon recovery.
    #[serde(skip)]
    partial: HashSet<NodeIndex>,

    pub(in crate::controller) tag_generator: usize,

    pub(crate) config: Config,
}

impl Materializations {
    /// Create a new set of materializations.
    pub(in crate::controller) fn new() -> Self {
        Materializations {
            have: HashMap::default(),
            had: HashSet::default(),
            added: HashMap::default(),
            new_readers: HashSet::default(),

            added_weak: HashMap::default(),

            paths: HashMap::default(),

            redundant_partial: HashMap::default(),

            partial: HashSet::default(),

            tag_generator: 0,

            config: Default::default(),
        }
    }

    /// Set the config for all future materializations
    pub(in crate::controller) fn set_config(&mut self, config: Config) {
        self.config = config;
    }

    /// Does this partial node have a fully materialized duplicate?
    pub(in crate::controller) fn get_redundant(&self, idx: &NodeIndex) -> Option<&NodeIndex> {
        self.redundant_partial.get(idx)
    }

    /// Add new duplicate nodes to the redundant_partial map
    pub(in crate::controller) fn extend_redundant_partial(
        &mut self,
        new_duplicates: HashMap<NodeIndex, NodeIndex>,
    ) {
        self.redundant_partial.extend(new_duplicates);
    }
}

impl Materializations {
    fn next_tag(&mut self) -> Tag {
        self.tag_generator += 1;
        Tag::new(self.tag_generator as u32)
    }

    fn tag_for_path(&mut self, index: &Index, path: &RawReplayPath) -> Tag {
        self.paths
            .get(&path.last_segment().node)
            .and_then(|paths_for_node| {
                paths_for_node.get_by_right(&(
                    index.clone(),
                    path.segments()
                        .iter()
                        .map(|segment| segment.node)
                        .collect::<Vec<_>>(),
                ))
            })
            .copied()
            .unwrap_or_else(|| self.next_tag())
    }

    /// Return a references to the set of indexes for the given node in the graph.
    ///
    /// If the node is not materialized, returns None.
    pub(crate) fn indexes_for(&self, ni: NodeIndex) -> Option<&HashSet<Index>> {
        self.have.get(&ni)
    }

    /// Is the given node partially materialized?
    ///
    /// Note that this method returns `false` if the node is fully materialized, *or* if it's not
    /// materialized at all
    pub(crate) fn is_partial(&self, node_index: NodeIndex) -> bool {
        self.partial.contains(&node_index)
    }

    /// Extend the current set of materializations with any additional materializations needed to
    /// satisfy indexing obligations in the given set of (new) nodes.
    #[allow(clippy::cognitive_complexity)]
    pub(in crate::controller) fn extend(
        &mut self,
        graph: &mut Graph,
        new: &HashSet<NodeIndex>,
        dmp: &DomainMigrationPlan,
    ) -> ReadySetResult<()> {
        let span = info_span!("materializations:extend");
        let _g = span.enter();
        // this code used to be a mess, and will likely be a mess this time around too.
        // but, let's try to start out in a principled way...
        //
        // we have a bunch of known existing materializations (self.have), and potentially a set of
        // newly added, but not yet constructed, materializations (self.added). Everything in
        // self.added is also in self.have. We're now being asked to compute any indexing
        // obligations created by the nodes in `nodes`, some of which may be new (iff the boolean
        // is true). `extend` will be called once per new domain, so it will be called several
        // times before `commit` is ultimately called to create the new materializations.
        //
        // There are multiple ways in which an indexing obligation can be created:
        //
        //  - a node can ask for its own state to be materialized
        //  - a node can indicate that it will perform lookups on its ancestors
        //  - a node can declare that it would benefit from an ancestor index for replays
        //
        // The last point is special, in that those indexes can be hoisted past *all* nodes,
        // including across domain boundaries. We call these "replay obligations". They are also
        // special in that they also need to be carried along all the way to the nearest *full*
        // materialization.
        //
        // In the first case, the materialization decision is easy: we materialize the node in
        // question. In the latter case, it is a bit more complex, since the parent may be in a
        // different domain, or may be a "query through" node that we want to avoid materializing.
        //
        // Computing indexing obligations is therefore a multi-stage process.
        //
        //  1. Compute what indexes each *new* operator requires.
        //  2. Add materializations for any lookup obligations, considering query-through.
        //  3. Recursively add indexes for replay obligations.
        //

        // Holds all lookup obligations. Keyed by the node that should be materialized.
        let mut lookup_obligations: HashMap<NodeIndex, HashSet<LookupIndex>> = HashMap::new();

        // Holds all replay obligations. Keyed by the node whose *parent* should be materialized.
        let mut replay_obligations: HashMap<NodeIndex, Indices> = HashMap::new();

        // Find indices we need to add.
        for &ni in new {
            let n = &graph[ni];

            let mut indices: HashMap<NodeIndex, IndexObligation> = if let Some(r) = n.as_reader() {
                if let Some(index) = r.index() {
                    // for a reader that will get lookups, we'd like to have an index above us
                    // somewhere on our key so that we can make the reader partial
                    self.new_readers.insert(ni);
                    HashMap::from([(ni, IndexObligation::Replay(index.clone()))])
                } else {
                    // only streaming, no indexing needed
                    continue;
                }
            } else {
                n.suggest_indexes(ni)
                    .into_iter()
                    .map(|(n, lookup_index)| (n, IndexObligation::Lookup(lookup_index)))
                    .collect()
            };

            if indices.is_empty() && n.is_base() {
                // we must *always* materialize base nodes
                // so, just make up some column to index on
                indices.insert(
                    ni,
                    IndexObligation::Lookup(LookupIndex::Strict(Index::hash_map(vec![0]))),
                );
            }

            for (ni, obligation) in indices {
                trace!(
                    node = %ni.index(),
                    obligation = ?obligation,
                    "new indexing obligation"
                );

                match obligation {
                    IndexObligation::Replay(index) => {
                        replay_obligations.entry(ni).or_default().insert(index);
                    }
                    IndexObligation::Lookup(index) => {
                        lookup_obligations.entry(ni).or_default().insert(index);
                    }
                }
            }
        }

        // map all the indices to the corresponding columns in the parent
        fn map_lookup_indices(
            n: &Node,
            parent: NodeIndex,
            indices: &HashSet<LookupIndex>,
        ) -> ReadySetResult<HashSet<LookupIndex>> {
            indices
                .iter()
                .map(|lookup_index| {
                    let index = lookup_index.index();
                    let index = Index::new(
                        index.index_type,
                        index
                            .columns
                            .iter()
                            .map(|&col| {
                                if !n.is_internal() {
                                    if n.is_base() {
                                        internal!("map_indices called with base table");
                                    }
                                    return Ok(col);
                                }

                                let really = n.parent_columns(col);
                                let really = really
                                    .into_iter()
                                    .find(|&(anc, _)| anc == parent)
                                    .and_then(|(_, col)| col);

                                really.ok_or_else(|| {
                                    internal_err!(
                                        "could not resolve obligation past operator;\
                                     node => {}, ancestor => {}, column => {}",
                                        n.global_addr().index(),
                                        parent.index(),
                                        col
                                    )
                                })
                            })
                            .collect::<ReadySetResult<Vec<usize>>>()?,
                    );
                    Ok(match lookup_index {
                        LookupIndex::Strict(_) => LookupIndex::Strict(index),
                        LookupIndex::Weak(_) => LookupIndex::Weak(index),
                    })
                })
                .collect()
        }

        // lookup obligations are fairly rigid, in that they require a materialization, and can
        // only be pushed through query-through nodes, and never across domains. so, we deal with
        // those first.
        //
        // it's also *important* that we do these first, because these are the only ones that can
        // force non-materialized nodes to become materialized. if we didn't do this first, a
        // partial node may add indices to only a subset of the intermediate partial views between
        // it and the nearest full materialization (because the intermediate ones haven't been
        // marked as materialized yet).
        for (ni, mut indices) in lookup_obligations {
            // we want to find the closest materialization that allows lookups (i.e., counting
            // query-through operators).
            let mut mi = ni;
            let mut m = &graph[mi];
            loop {
                if self.have.contains_key(&mi) {
                    break;
                }
                if !m.is_internal() || !m.can_query_through() {
                    break;
                }

                let mut parents = graph.neighbors_directed(mi, petgraph::EdgeDirection::Incoming);
                #[allow(clippy::unwrap_used)] // parent must exist because node is internal
                let parent = parents.next().unwrap();
                assert_eq!(
                    parents.count(),
                    0,
                    "query_through had more than one ancestor"
                );

                // hoist index to parent
                trace!(
                    for_node = %mi.index(),
                    to_node  = %parent.index(),
                    "hoisting indexing obligations"
                );
                mi = parent;
                indices = map_lookup_indices(m, mi, &indices)?;
                m = &graph[mi];
            }

            for index in indices {
                debug!(
                    node = %mi.index(),
                    ?index,
                    "adding lookup index to view"
                );

                // Since lookups into weak indices are forbidden when processing replays, any weak
                // index that we add needs to *also* have a corresponding strict index of the same
                // type and columns.
                if index.is_weak() {
                    self.added_weak
                        .entry(mi)
                        .or_default()
                        .insert(index.index().clone());
                }

                if self
                    .added
                    .entry(mi)
                    .or_default()
                    .insert(index.index().clone())
                {
                    self.have
                        .entry(mi)
                        .or_default()
                        .insert(index.index().clone());

                    // also add a replay obligation to enable partial
                    replay_obligations
                        .entry(mi)
                        .or_default()
                        .insert(index.into_index());
                }
            }
        }

        // we need to compute which views can be partial, and which can not.
        // in addition, we need to figure out what indexes each view should have.
        // this is surprisingly difficult to get right.
        //
        // the approach we are going to take is to require walking the graph bottom-up:
        let mut ordered = Vec::with_capacity(graph.node_count());
        let mut topo = petgraph::visit::Topo::new(graph as &Graph);
        while let Some(node) = topo.next(graph as &Graph) {
            if graph[node].is_source() {
                continue;
            }
            if graph[node].is_dropped() {
                continue;
            }

            // unfortunately, we may end up adding indexes to existing views, and we need to walk
            // them *all* in reverse topological order.
            ordered.push(node);
        }
        ordered.reverse();
        // for each node, we will check if it has any *new* indexes (i.e., in self.added).
        // if it does, see if the indexed columns resolve into its nearest ancestor
        // materializations. if they do, we mark this view as partial. if not, we, well, don't.
        // if the view was marked as partial, we add the necessary indexes to self.added for the
        // parent views, and keep walking. this is the reason we need the reverse topological
        // order: if we didn't, a node could receive additional indexes after we've checked it!
        for ni in ordered {
            let indexes = match replay_obligations.remove(&ni) {
                Some(idxs) => idxs,
                None => continue,
            };

            // we want to find out if it's possible to partially materialize this node. for that to
            // be the case, we need to keep moving up the ancestor tree of `ni`, and check at each
            // stage that we can trace the key column back into each of our nearest
            // materializations.
            let mut able = self.config.partial_enabled;
            let mut add = HashMap::new();

            // bases can't be partial
            if graph[ni].is_base() {
                able = false;
            }

            if graph[ni].is_internal() && graph[ni].requires_full_materialization() {
                debug!(node = %ni.index(), "full because required");
                able = false;
            }

            // we are already fully materialized, so can't be made partial
            if !new.contains(&ni)
                && self.added.get(&ni).map(|i| i.len()).unwrap_or(0)
                    != self.have.get(&ni).map(|i| i.len()).unwrap_or(0)
                && !self.partial.contains(&ni)
            {
                debug!(node = %ni.index(), "cannot turn full into partial");
                able = false;
            }

            // do we have a full materialization below us?
            let mut stack: Vec<_> = graph
                .neighbors_directed(ni, petgraph::EdgeDirection::Outgoing)
                .collect();

            while let Some(child) = stack.pop() {
                // allow views to force full (XXX)
                if graph[child].name().name.starts_with("FULL_") {
                    stack.clear();
                    able = false;
                }

                if self.have.contains_key(&child) {
                    // materialized child -- don't need to keep walking along this path
                    if !self.partial.contains(&child) {
                        // child is full, so we can't be partial
                        debug!(node = %ni.index(), child = %child.index(), "full because descendant is full");
                        stack.clear();
                        able = false
                    }
                } else if graph[child].as_reader().and_then(|r| r.key()).is_some() {
                    // reader child (which is effectively materialized)
                    if !self.partial.contains(&child) {
                        // reader is full, so we can't be partial
                        debug!(node = %ni.index(), reader = %child.index(), "full because reader below is full");
                        stack.clear();
                        able = false
                    }
                } else {
                    // non-materialized child -- keep walking
                    stack
                        .extend(graph.neighbors_directed(child, petgraph::EdgeDirection::Outgoing));
                }
            }

            // Figure out the set of paths needed to reconstruct each of the indexes
            let mut paths = vec![];
            for index in &indexes {
                #[allow(clippy::unwrap_used)] // index.columns cannot be empty
                paths.extend(keys::replay_paths_for_nonstop(
                    graph,
                    ColumnRef {
                        node: ni,
                        columns: index.columns.clone(),
                    },
                    index.index_type,
                )?);
            }

            // Uniquely, broken paths (paths which terminate early at a set of columns that're
            // generated by a node) have the ability to force a node to be materialized. We need to
            // look at these first, since subsequent paths would then want to stop at those newly
            // materialized nodes (otherwise, we'd end up having a path that goes *through* a
            // materialization, which confuses the bit that actually generates the replay paths
            // later!)
            paths.sort_unstable_by_key(|p| !p.broken());

            'paths: for path in paths {
                // Some of these replay paths might start at nodes other than the one we're
                // passing to replay_paths_for, if generated columns are involved. We need to
                // materialize those nodes, too.
                let n_to_skip = usize::from(path.target().node == ni);

                // Iterate *up* the path (in reverse order) until we either determine that we need
                // to be fully materialized, or we hit an existing materialization that we need to
                // add an index to
                for (i, IndexRef { node, index }) in
                    path.segments().iter().rev().enumerate().skip(n_to_skip)
                {
                    match index {
                        None => {
                            debug!(
                                node = %node.index(),
                                "full because node before requested full replay",
                            );
                            able = false;
                            break 'paths;
                        }
                        Some(index) => {
                            if let Some(m) = self.have.get(node) {
                                // We've found an already-materialized node along our path - we can
                                // use that as the source of our eventual replay path
                                if !m.contains(index) {
                                    // we need to add an index to this materialization to make that
                                    // happen
                                    add.entry(*node)
                                        .or_insert_with(HashSet::new)
                                        .insert(index.clone());
                                }
                                break;
                            }
                            if i == path.len() - 1 && path.broken() {
                                self.have.entry(*node).or_insert_with(|| {
                                    debug!(node = %node.index(), "forcing materialization for node with generated columns");
                                    HashSet::new()
                                });

                                add.entry(*node)
                                    .or_insert_with(HashSet::new)
                                    .insert(index.clone());
                            }
                        }
                    }
                }
            }

            if able {
                // we can do partial if we add all those indices!
                self.partial.insert(ni);
                debug!(node = %ni.index(), "using partial materialization");
                for (mi, indices) in add {
                    replay_obligations.entry(mi).or_default().extend(indices);
                }
            } else if !graph[ni].is_base() && !self.config.allow_full_materialization {
                unsupported!(
                    "Creation of fully materialized query is disabled \
                     (node {} / {} / {}  would be fully materialized)",
                    ni.index(),
                    graph[ni].name().display_unquoted(),
                    graph[ni].description(true),
                );
            } else {
                invariant!(
                    !graph[ni].purge,
                    "full materialization placed beyond materialization frontier"
                );
            }

            // no matter what happens, we're going to have to fulfill our replay obligations.
            if let Some(m) = self.have.get_mut(&ni) {
                for index in indexes {
                    let new_index = m.insert(index.clone());

                    if new_index {
                        debug!(
                          on = %ni.index(),
                          columns = ?index,
                          "adding index to view to enable partial"
                        );
                    }

                    if new_index || self.partial.contains(&ni) || dmp.is_recovery() {
                        // we need to add to self.added even if we didn't explicitly add any new
                        // indices if we're partial, because existing domains will need to be told
                        // about new partial replay paths sourced from this node.
                        self.added.entry(ni).or_default().insert(index);
                    }
                }
            }
        }
        assert!(replay_obligations.is_empty());

        // Mark nodes as beyond the frontier as dictated by the strategy
        for &ni in new {
            #[allow(clippy::unwrap_used)] // graph must contain nodes in new
            let n = graph.node_weight_mut(ni).unwrap();

            if (self.have.contains_key(&ni) || n.is_reader()) && !self.partial.contains(&ni) {
                // full materializations cannot be beyond the frontier.
                continue;
            }

            if n.name().name.starts_with("SHALLOW_") {
                n.purge = true;
                continue;
            }

            // For all other strategies, we only want to deal with partial indices
            if !self.partial.contains(&ni) {
                continue;
            }

            if let FrontierStrategy::AllPartial = self.config.frontier_strategy {
                n.purge = true;
            } else if let FrontierStrategy::Readers = self.config.frontier_strategy {
                n.purge = n.purge || n.is_reader();
            }
        }

        for &ni in new {
            // any nodes marked as .purge should have their state be beyond the materialization
            // frontier. however, mir may have named an identity child instead of the node with a
            // materialization, so let's make sure the label gets correctly applied: specifically,
            // if a .prune node doesn't have state, we "move" that .prune to its ancestors.
            if graph[ni].purge && !(self.have.contains_key(&ni) || graph[ni].is_reader()) {
                let mut it = graph
                    .neighbors_directed(ni, petgraph::EdgeDirection::Incoming)
                    .detach();
                while let Some((_, pi)) = it.next(&*graph) {
                    if !new.contains(&pi) {
                        continue;
                    }
                    if !self.have.contains_key(&pi) {
                        debug!(node = %ni.index(), "no associated state with purged node");
                        continue;
                    }
                    invariant!(
                        self.partial.contains(&pi),
                        "attempting to place full materialization beyond materialization frontier"
                    );
                    // #[allow(clippy::unwrap_used)] // graph must contain pi
                    graph.node_weight_mut(pi).unwrap().purge = true;
                }
            }
        }

        Ok(())
    }

    /// Retrieves the materialization status of a given node, or None
    /// if the node isn't materialized.
    pub(in crate::controller) fn get_status(
        &self,
        index: NodeIndex,
        node: &Node,
    ) -> MaterializationStatus {
        let is_materialized = self.have.contains_key(&index)
            || node
                .as_reader()
                .map(|r| r.is_materialized())
                .unwrap_or(false);

        if !is_materialized {
            MaterializationStatus::Not
        } else if self.partial.contains(&index) {
            MaterializationStatus::Partial {
                beyond_materialization_frontier: node.purge,
            }
        } else {
            MaterializationStatus::Full
        }
    }

    /// Construct an iterator over the indexes of non-reader nodes that are materialized.
    pub(in crate::controller) fn materialized_non_reader_nodes(
        &self,
    ) -> impl Iterator<Item = NodeIndex> + '_ {
        self.have.keys().copied()
    }

    /// validate all graph invariants for the materializations in `self` for all nodes in `new` in
    /// the given `graph`, returning an `Err` if any invariants are violated. This consists of:
    ///
    /// * Checking to make sure no partially materialized nodes exist that are ancestors of fully
    ///   materialized nodes
    /// * Checking that no node is partial over a subset of the indices in its parent
    /// * Checking that there are no cases where a subgraph is sharded by one column, and then has a
    ///   replay path on a duplicated copy of that column.
    ///
    /// If the validation fails because a full node is detected below a partial node, InvalidEdge
    /// is returned to indicate which edge must be recreated in the migration planning loop.
    pub(super) fn validate(
        &self,
        graph: &Graph,
        new: &HashSet<NodeIndex>,
    ) -> ReadySetResult<Option<InvalidEdge>> {
        // check that we don't have fully materialized nodes downstream of partially materialized
        // nodes.
        // returns (parent_index, child_index) if two neighbors are found where parent is partially
        // materialized and child is fully materialized.
        {
            fn any_partial(
                this: &Materializations,
                graph: &Graph,
                ni: NodeIndex,
            ) -> (Option<NodeIndex>, Option<NodeIndex>) {
                if this.partial.contains(&ni) {
                    return (Some(ni), None);
                }
                for pi in graph.neighbors_directed(ni, petgraph::EdgeDirection::Incoming) {
                    match any_partial(this, graph, pi) {
                        (Some(pi), Some(ni)) => return (Some(pi), Some(ni)),
                        (Some(pi), None) => return (Some(pi), Some(ni)),
                        _ => {}
                    }
                }
                (None, None)
            }

            for ni in self.added.keys().copied().chain(self.new_readers.clone()) {
                if let (Some(pi), Some(ni)) = any_partial(self, graph, ni) {
                    return Ok(Some(InvalidEdge {
                        parent: pi,
                        child: ni,
                    }));
                }
            }
        }

        // check that no node is partial over a subset of the indices in its parent
        {
            for (&ni, added) in &self.added {
                if !self.partial.contains(&ni) {
                    continue;
                }

                for index in added {
                    #[allow(clippy::unwrap_used)] // index.columns cannot be empty
                    let paths = keys::replay_paths_for_nonstop(
                        graph,
                        ColumnRef {
                            node: ni,
                            columns: index.columns.clone(),
                        },
                        index.index_type,
                    )?;

                    for path in paths {
                        for IndexRef { node, index } in path.segments().iter().rev() {
                            match index {
                                None => break,
                                Some(child_index) => {
                                    if self.partial.contains(node) {
                                        // self.partial should be a subset of self.have
                                        'outer: for parent_index in &self.have[node] {
                                            // is this node partial over some of the child's partial
                                            // columns, but not others? if so, we run into really
                                            // sad
                                            // situations where the parent could miss in its state
                                            // despite
                                            // the child having state present for that key.

                                            // Are the indexes the same type?
                                            if parent_index.index_type != child_index.index_type {
                                                continue;
                                            }

                                            // do we share a column?
                                            if parent_index
                                                .columns
                                                .iter()
                                                .all(|&c| !child_index.columns.contains(&c))
                                            {
                                                continue;
                                            }

                                            // is there a column we *don't* share?
                                            let unshared =
                                                parent_index
                                                    .columns
                                                    .iter()
                                                    .cloned()
                                                    .find(|&c| !child_index.columns.contains(&c))
                                                    .or_else(|| {
                                                        child_index.columns.iter().cloned().find(
                                                            |c| !parent_index.columns.contains(c),
                                                        )
                                                    });
                                            if let Some(not_shared) = unshared {
                                                // This might be fine if we also have the child's
                                                // index in
                                                // the parent, since then the overlapping index
                                                // logic in
                                                // `MemoryState::lookup` will save us.

                                                for other_idx in &self.have[node] {
                                                    if other_idx == child_index {
                                                        // Looks like we have the necessary index,
                                                        // so we'll
                                                        // be okay.
                                                        continue 'outer;
                                                    }
                                                }
                                                // If we get here, we've somehow managed to not
                                                // index the
                                                // parent by the same key as the child, which really
                                                // should
                                                // never happen.
                                                // This code should probably just be taken out soon.
                                                println!(
                                                    "{}",
                                                    Graphviz {
                                                        graph,
                                                        detailed: true,
                                                        node_sizes: None,
                                                        materializations: self,
                                                        domain_nodes: None,
                                                        reachable_from: None,
                                                    }
                                                );
                                                error!(
                                                    parent = %node.index(),
                                                    parent_index = ?parent_index,
                                                    child = %ni.index(),
                                                    child_index = ?child_index,
                                                    conflict = not_shared,
                                                    "partially lapping partial indices"
                                                );
                                                internal!(
                                                    "partially overlapping partial indices (parent {:?} cols {:?} all {:?}, child {:?} cols {:?})",
                                                    node.index(), parent_index, &self.have[node], ni.index(), parent_index
                                                );
                                            }
                                        }
                                    } else if self.have.contains_key(&ni) {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // check that we never have non-purge below purge
            let mut non_purge = Vec::new();
            for &ni in new {
                if (graph[ni].is_reader() || self.have.contains_key(&ni)) && !graph[ni].purge {
                    for pi in graph.neighbors_directed(ni, petgraph::EdgeDirection::Incoming) {
                        non_purge.push(pi);
                    }
                }
            }
            while let Some(ni) = non_purge.pop() {
                if graph[ni].purge {
                    println!(
                        "{}",
                        Graphviz {
                            graph,
                            detailed: true,
                            node_sizes: None,
                            materializations: self,
                            domain_nodes: None,
                            reachable_from: None,
                        }
                    );
                    internal!("found purge node {} above non-purge node", ni.index())
                }
                if self.have.contains_key(&ni) {
                    // already shceduled to be checked
                    // NOTE: no need to check for readers here, since they can't be parents
                    continue;
                }
                for pi in graph.neighbors_directed(ni, petgraph::EdgeDirection::Incoming) {
                    non_purge.push(pi);
                }
            }
            drop(non_purge);
        }

        // check that we don't have any cases where a subgraph is sharded by one column, and then
        // has a replay path on a duplicated copy of that column. for example, a join with
        // [B(0, 0), R(0)] where the join's subgraph is sharded by .0, but a downstream replay path
        // looks up by .1. this causes terrible confusion where the target (correctly) queries only
        // one shard, but the shard merger expects to have to wait for all shards (since the replay
        // key and the sharding key do not match at the shard merger).
        {
            for &node in new {
                let n = &graph[node];
                if !n.is_shard_merger() {
                    continue;
                }

                // we don't actually store replay paths anywhere in Materializations (perhaps we
                // should). however, we can check a proxy for the necessary property by making sure
                // that our parent's sharding key is never aliased. this will lead to some false
                // positives (all replay paths may use the same alias as we shard by), but we'll
                // deal with that.
                let parent = graph
                    .neighbors_directed(node, petgraph::EdgeDirection::Incoming)
                    .next()
                    .ok_or_else(|| internal_err!("shard mergers must have a parent"))?;
                let psharding = graph[parent].sharded_by();

                if let Sharding::ByColumn(col, _) = psharding {
                    // we want to resolve col all the way to its nearest materialized ancestor.
                    // and then check whether any other cols of the parent alias that source column
                    let columns: Vec<_> = (0..n.columns().len()).collect();
                    for path in keys::provenance_of(graph, parent, &columns[..])? {
                        let (mat_anc, cols) = path
                            .into_iter()
                            .find(|&(n, _)| self.have.contains_key(&n))
                            .ok_or_else(|| {
                                internal_err!(
                                    "since bases are materialized, \
                                 every path must eventually have a materialized node",
                                )
                            })?;
                        let src = cols[col];
                        if src.is_none() {
                            continue;
                        }

                        if let Some((c, res)) = cols
                            .iter()
                            .enumerate()
                            .find(|&(c, res)| c != col && res == &src)
                        {
                            // another column in the merger's parent resolved to the source column!
                            //println!("{}", graphviz(graph, &self));
                            error!(
                                parent = %mat_anc.index(),
                                aliased = ?res,
                                sharded = %parent.index(),
                                alias = c,
                                shard = col,
                                "attempting to merge sharding by aliased column"
                            );
                            internal!("attempting to merge sharding by aliased column (parent {:?}, aliased {:?}, sharded {:?}, alias {:?}, shard {:?})", mat_anc.index(), res, parent.index(), c, col)
                        }
                    }
                }
            }
        }

        Ok(None)
    }

    /// Commit to all materialization decisions since the last time `commit` was called.
    ///
    /// This includes setting up replay paths, adding new indices to existing materializations, and
    /// populating new materializations.
    #[allow(clippy::cognitive_complexity)]
    pub(in crate::controller) fn commit(
        &mut self,
        graph: &mut Graph,
        new: &HashSet<NodeIndex>,
        dmp: &mut DomainMigrationPlan,
    ) -> Result<(), ReadySetError> {
        let mut reindex = Vec::with_capacity(new.len());
        let mut make = Vec::with_capacity(new.len());
        let mut topo = petgraph::visit::Topo::new(&*graph);
        while let Some(node) = topo.next(&*graph) {
            if graph[node].is_source() {
                continue;
            }
            if graph[node].is_dropped() {
                continue;
            }

            if new.contains(&node) {
                make.push(node);
            } else if self.added.contains_key(&node) {
                reindex.push(node);
            }
        }

        // Track a set of nodes which we haven't already waited to be ready
        let mut non_ready_nodes = make
            .iter()
            .copied()
            .map(|n| (graph[n].domain(), graph[n].local_addr()))
            .collect::<HashSet<_>>();

        // first, we add any new indices to existing nodes
        for node in reindex {
            let mut index_on = self.added.remove(&node).unwrap();

            // are they trying to make a non-materialized node materialized?
            if !self.had.contains(&node) && !index_on.is_empty() {
                if self.partial.contains(&node) {
                    // we can't make this node partial if any of its children are materialized, as
                    // we might stop forwarding updates to them, which would make them very sad.
                    //
                    // the exception to this is for new children, or old children that are now
                    // becoming materialized; those are necessarily empty, and so we won't be
                    // violating key monotonicity.
                    //
                    // NOTE(aspen): We haven't actually seen this happen in the real world yet, but
                    // it might be possible, especially once we bring back reuse. If we do start
                    // seeing this (and we're not just seeing it because of a bug like #421), there
                    // are a couple of options here:
                    //
                    // 1. We could split the graph at this point similar to what we do for the
                    //    full-below-partial case (see `validate`)
                    // 2. We could always send evictions downstream of nodes that become newly
                    //    partially materialized
                    //
                    // I'm personally partial (ha!) to the second option because it feels *always*
                    // correct in an elegant way and also creates smaller graphs with fewer
                    // materializations, but there might be some weirdness I'm not thinking of. But
                    // this also might just be impossible anyway, which makes this all moot.
                    let mut stack: Vec<_> = graph
                        .neighbors_directed(node, petgraph::EdgeDirection::Outgoing)
                        .collect();
                    while let Some(child) = stack.pop() {
                        if new.contains(&child) {
                            // NOTE: no need to check its children either
                            continue;
                        }

                        if self.added.get(&child).map(|i| i.len()).unwrap_or(0)
                            != self.have.get(&child).map(|i| i.len()).unwrap_or(0)
                        {
                            // node was previously materialized!
                            eprintln!(
                                "{}",
                                Graphviz {
                                    graph,
                                    detailed: true,
                                    node_sizes: None,
                                    materializations: self,
                                    domain_nodes: None,
                                    reachable_from: None,
                                }
                            );
                            error!(
                                node = %node.index(),
                                child = %child.index(),
                                "attempting to make old non-materialized node with children partial"
                            );
                            internal!("attempting to make old non-materialized node ({:?}) with child ({:?}) partial", node.index(), child.index());
                        }

                        stack.extend(
                            graph.neighbors_directed(child, petgraph::EdgeDirection::Outgoing),
                        );
                    }
                }

                debug!(
                    node = %node.index(),
                    cols = ?index_on,
                    "materializing existing non-materialized node"
                );
            }

            let n = &graph[node];
            if self.partial.contains(&node) {
                debug!(
                    node = %node.index(),
                    cols = ?index_on,
                    "adding partial index to existing {:?}", n
                );
            }
            // We attempt to maintain the invariant that the materialization planner is always run
            // for every new added index, because replays might need to be done (or replay paths
            // set up, if we're partial).
            // This is somewhat wasteful in some (fully materialized) cases, but it's a lot easier
            // to reason about if all the replay decisions happen in the planner.
            {
                let span = info_span!("reconstructing node", node = %node.index());
                let _guard = span.enter();
                self.setup(node, &mut index_on, &mut non_ready_nodes, graph, dmp)?;
            }
            index_on.clear();
        }

        // then, we start prepping new nodes
        for ni in &make {
            let n = &graph[*ni];
            let mut index_on = self
                .added
                .remove(ni)
                .map(|idxs| -> ReadySetResult<_> {
                    invariant!(!idxs.is_empty());
                    Ok(idxs)
                })
                .transpose()?
                .unwrap_or_default();

            let start = ::std::time::Instant::now();
            self.ready_one(*ni, &mut index_on, &mut non_ready_nodes, graph, dmp)?;
            let reconstructed = index_on.is_empty();

            // communicate to the domain in charge of a particular node that it should start
            // delivering updates to a given new node. note that we wait for the domain to
            // acknowledge the change. this is important so that we don't ready a child in a
            // different domain before the parent has been readied. it's also important to avoid us
            // returning before the graph is actually fully operational.
            trace!(node = %ni.index(), "readying node");
            dmp.add_message(
                n.domain(),
                DomainRequest::Ready {
                    node: n.local_addr(),
                    purge: n.purge,
                    index: index_on,
                },
            )?;
            trace!(node = %ni.index(), "node ready");

            if reconstructed {
                debug!(
                    ms = %start.elapsed().as_millis(),
                    node = %ni.index(),
                    "reconstruction completed"
                );
            }
        }

        // Wait for each of the nodes to be ready which we didn't already (eg because we wanted to
        // replay from them)
        for (domain, node) in non_ready_nodes {
            dmp.add_message(domain, DomainRequest::IsReady { node })?;
        }

        self.added.clear();
        self.new_readers.clear();
        self.had.extend(self.have.keys().copied());
        Ok(())
    }

    /// Perform all operations necessary to bring any materializations for the given node up, and
    /// then mark that node as ready to receive updates.
    fn ready_one(
        &mut self,
        ni: NodeIndex,
        index_on: &mut Indices,
        non_ready_nodes: &mut HashSet<(DomainIndex, LocalNodeIndex)>,
        graph: &Graph,
        dmp: &mut DomainMigrationPlan,
    ) -> Result<(), ReadySetError> {
        let n = &graph[ni];
        let mut has_state = !index_on.is_empty();

        if has_state {
            if self.partial.contains(&ni) {
                debug!("new partially-materialized node: {:?}", n);
            } else {
                debug!("new fully-materalized node: {:?}", n);
            }
        } else {
            debug!("new stateless node: {:?}", n);
        }

        if n.is_base() {
            // a new base must be empty, so we can materialize it immediately
            debug!(node = %ni.index(), "no need to replay empty new base");
            assert!(!self.partial.contains(&ni));
            return Ok(());
        }

        // if this node doesn't need to be materialized, then we're done.
        has_state = !index_on.is_empty();
        if let Some(r) = n.as_reader() {
            if r.is_materialized() {
                has_state = true;
            }
        }

        if !has_state {
            debug!(node = %ni.index(), "no need to replay non-materialized view");
            return Ok(());
        }

        // we have a parent that has data, so we need to replay and reconstruct
        {
            let span = info_span!("reconstructing node", node = %ni.index());
            let _guard = span.enter();
            debug!(node = %ni.index(), "beginning reconstruction");
            self.setup(ni, index_on, non_ready_nodes, graph, dmp)?;
        }

        // NOTE: the state has already been marked ready by the replay completing, but we want to
        // wait for the domain to finish replay, which the ready executed by the outer commit()
        // loop does.
        index_on.clear();
        Ok(())
    }

    /// Reconstruct the materialized state required by the given (new) node through replay.
    fn setup(
        &mut self,
        ni: NodeIndex,
        index_on: &mut Indices,
        non_ready_nodes: &mut HashSet<(DomainIndex, LocalNodeIndex)>,
        graph: &Graph,
        dmp: &mut DomainMigrationPlan,
    ) -> Result<(), ReadySetError> {
        if index_on.is_empty() {
            // we must be reconstructing a Reader.
            // figure out what key that Reader is using
            if let Some(r) = graph[ni].as_reader() {
                invariant!(r.is_materialized());
                if let Some(index) = r.index() {
                    index_on.insert(index.clone());
                }
            } else {
                internal!("index_on cannot be empty for a non-Reader node")
            }
        }

        // construct and disseminate a plan for each index
        let (pending, paths) = {
            let mut plan = plan::Plan::new(self, graph, ni, dmp);
            for index in index_on.drain() {
                plan.add(index)?;
            }
            plan.finalize()?
        };
        // grr `HashMap` doesn't implement `IndexMut`
        self.paths.entry(ni).or_default().extend(paths);

        if pending.is_empty() {
            trace!("No replays to do");
        } else {
            trace!("all domains ready for replay");
            // prepare for, start, and wait for replays
            for pending in pending {
                // tell the first domain to start playing
                debug!(
                    domain = %pending.source_domain.index(),
                    "telling root domain to start replay"
                );

                // Before we try to replay from the source node, wait for it to be ready (but only
                // if we haven't done so already)
                if non_ready_nodes.remove(&(pending.source_domain, pending.source)) {
                    dmp.add_message(
                        pending.source_domain,
                        DomainRequest::IsReady {
                            node: pending.source,
                        },
                    )?;
                }

                dmp.add_message(
                    pending.source_domain,
                    DomainRequest::StartReplay {
                        tag: pending.tag,
                        from: pending.source,
                        replicas: None,
                        targeting_domain: pending.target_domain,
                    },
                )?;
            }
            // and then wait for the last domain to receive all the records
            let target = graph[ni].domain();
            debug!(
               domain = %target.index(),
               "waiting for done message from target"
            );
            dmp.add_message(
                target,
                DomainRequest::QueryReplayDone {
                    node: graph[ni].local_addr(),
                },
            )?;
        }
        Ok(())
    }

    /// Returns a (`NodeIndex`, `Tag`) pair for each index in a partially materialized node.
    pub(in crate::controller) fn partial_tags(&self) -> Vec<(NodeIndex, Tag)> {
        // For each partially materialized node, get each tag in self::paths
        #[allow(clippy::unwrap_used)]
        self.partial
            .iter()
            .filter_map(|partial_node| {
                // Each replay path for a partial index on `partial_node`
                self.paths
                    .get(partial_node)
                    .map(|tags| (partial_node, tags))
            })
            .flat_map(|(partial_node, tags)| tags.iter().map(|(tag, _)| (*partial_node, *tag)))
            .collect()
    }
}
