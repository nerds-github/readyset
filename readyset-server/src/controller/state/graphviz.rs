use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt::{self, Display};

use dataflow::prelude::{Graph, NodeIndex};
use dataflow::{DomainIndex, NodeMap};
use lazy_static::lazy_static;
use petgraph::Direction;
use readyset_client::debug::info::NodeSize;
use regex::Regex;

use crate::controller::migrate::materialization::Materializations;

#[allow(clippy::unwrap_used)] // regex is hardcoded and valid
fn sanitize(s: &str) -> Cow<str> {
    lazy_static! {
        static ref SANITIZE_RE: Regex = Regex::new("([<>])").unwrap();
    };
    SANITIZE_RE.replace_all(s, "\\$1")
}

pub(in crate::controller) struct Graphviz<'a> {
    pub graph: &'a Graph,
    pub detailed: bool,
    pub node_sizes: Option<HashMap<NodeIndex, NodeSize>>,
    pub materializations: &'a Materializations,
    pub domain_nodes: Option<&'a HashMap<DomainIndex, NodeMap<NodeIndex>>>,
    pub reachable_from: Option<(NodeIndex, Direction)>,
}

/// Builds a graphviz [dot][] representation of the graph
///
/// For more information, see <http://docs/debugging.html#graphviz>
///
/// [dot]: https://graphviz.org/doc/info/lang.html
impl Display for Graphviz<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let indentln = |f: &mut fmt::Formatter<'_>| f.write_str("    ");
        let node_sizes = self.node_sizes.clone().unwrap_or_default();

        // header.
        writeln!(f, "digraph {{")?;

        // global formatting.
        indentln(f)?;
        write!(f, "fontsize=10")?;
        indentln(f)?;
        if self.detailed {
            writeln!(f, "node [shape=record, fontsize=10]")?;
        } else {
            writeln!(
                f,
                "graph [ fontsize=24 fontcolor=\"#0C6fA9\", outputorder=edgesfirst ]"
            )?;
            writeln!(f, "edge [ color=\"#0C6fA9\", style=bold ]")?;
            writeln!(
                f,
                "node [ color=\"#0C6fA9\", shape=box, style=\"rounded,bold\" ]"
            )?;
        }

        let nodes = if let Some((ni, dir)) = self.reachable_from {
            let mut nodes = HashSet::new();
            let mut stack = vec![ni];
            while let Some(node) = stack.pop() {
                if nodes.insert(node) {
                    for next in self.graph.neighbors_directed(node, dir) {
                        if !nodes.contains(&next) {
                            stack.push(next);
                        }
                    }
                }
            }

            nodes
        } else {
            self.graph.node_indices().collect()
        };

        let domain_for_node = self
            .domain_nodes
            .iter()
            .flat_map(|m| m.iter())
            .flat_map(|(di, nodes)| nodes.iter().map(|(_, ni)| (*ni, *di)))
            .collect::<HashMap<_, _>>();
        let mut domains_to_nodes = HashMap::new();
        for ni in &nodes {
            let domain = domain_for_node.get(ni).copied();
            domains_to_nodes
                .entry(domain)
                .or_insert_with(Vec::new)
                .push(*ni);
        }

        // node descriptions.
        for (domain, nodes) in domains_to_nodes {
            if let Some(domain) = domain {
                indentln(f)?;
                write!(
                    f,
                    "subgraph cluster_d{domain} {{\n    \
                 label = \"Domain {domain}\";\n    \
                 style=filled;\n    \
                 color=grey97;\n    "
                )?;
            }
            for index in nodes {
                let node = &self.graph[index];
                let materialization_status = self.materializations.get_status(index, node);
                indentln(f)?;
                write!(f, "n{}", index.index())?;
                write!(
                    f,
                    "{}",
                    sanitize(&node.describe(
                        index,
                        self.detailed,
                        &node_sizes,
                        materialization_status
                    ))
                    .as_ref(),
                )?;
            }
            if domain.is_some() {
                write!(f, "\n    }}\n")?;
            }
        }

        // edges.
        for edge in self.graph.raw_edges() {
            if !(nodes.contains(&edge.source()) && nodes.contains(&edge.target())) {
                continue;
            }

            indentln(f)?;
            write!(
                f,
                "n{} -> n{} [ {} ]",
                edge.source().index(),
                edge.target().index(),
                if self.graph[edge.source()].is_egress() {
                    "color=\"#CCCCCC\""
                } else if self.graph[edge.source()].is_source() {
                    "style=invis"
                } else {
                    ""
                }
            )?;
            writeln!(f)?;
        }

        // footer.
        write!(f, "}}")
    }
}
