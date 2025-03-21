use std::collections::HashMap;
use std::convert::TryFrom;
use std::str::FromStr;
use std::time::Instant;

use anyhow::Result;
use clap::Parser;
use database_utils::{DatabaseConnection, DatabaseURL, QueryableConnection};
use metrics::Unit;
use serde::{Deserialize, Serialize};

use crate::benchmark::{BenchmarkControl, BenchmarkResults, DeploymentParameters, MetricGoal};
use crate::benchmark_histogram;
use crate::utils::generate::DataGenerator;
use crate::utils::prometheus::ForwardPrometheusMetrics;
use crate::utils::query::{ArbitraryQueryParameters, CachingQueryGenerator};

/// Measure query execution time for both cache hits and cache misses of a single query
#[derive(Parser, Clone, Serialize, Deserialize)]
pub struct CacheHitBenchmark {
    /// Parameters to handle generating parameters for arbitrary queries.
    #[command(flatten)]
    query: ArbitraryQueryParameters,

    /// Install and generate from an arbitrary schema.
    #[command(flatten)]
    data_generator: DataGenerator,

    /// Number of cache hits to perform
    #[arg(long, default_value = "1000")]
    num_cache_hits: u32,

    /// Number of cache misses to perform
    #[arg(long, default_value = "1000")]
    num_cache_misses: u32,
}

impl BenchmarkControl for CacheHitBenchmark {
    async fn setup(&self, deployment: &DeploymentParameters) -> Result<()> {
        self.data_generator
            .install(&deployment.setup_conn_str)
            .await?;
        self.data_generator
            .generate(&deployment.setup_conn_str)
            .await?;
        Ok(())
    }

    async fn reset(&self, deployment: &DeploymentParameters) -> Result<()> {
        let mut conn = DatabaseURL::from_str(&deployment.target_conn_str)?
            .connect(None)
            .await?;
        let _ = self.query.unmigrate(&mut conn).await;
        Ok(())
    }

    async fn benchmark(&self, deployment: &DeploymentParameters) -> Result<BenchmarkResults> {
        // Explicitly migrate the query before benchmarking.
        let mut conn = DatabaseURL::from_str(&deployment.target_conn_str)?
            .connect(None)
            .await?;
        self.query.migrate(&mut conn).await?;

        let mut gen = CachingQueryGenerator::from(self.query.prepared_statement(&mut conn).await?);
        let mut results = BenchmarkResults::new();

        // Generate the cache misses.
        self.run_queries(&mut conn, &mut gen, true, &mut results)
            .await?;
        // Generate the cache hits.
        self.run_queries(&mut conn, &mut gen, false, &mut results)
            .await?;

        Ok(results)
    }

    fn labels(&self) -> HashMap<String, String> {
        let mut labels = HashMap::new();
        labels.extend(self.query.labels());
        labels.extend(self.data_generator.labels());
        labels
    }

    fn forward_metrics(&self, _: &DeploymentParameters) -> Vec<ForwardPrometheusMetrics> {
        vec![]
    }

    fn name(&self) -> &'static str {
        "cache_hit_benchmark"
    }

    fn data_generator(&mut self) -> Option<&mut DataGenerator> {
        Some(&mut self.data_generator)
    }
}

impl CacheHitBenchmark {
    async fn run_queries(
        &self,
        conn: &mut DatabaseConnection,
        gen: &mut CachingQueryGenerator,
        cache_miss: bool,
        results: &mut BenchmarkResults,
    ) -> Result<()> {
        // Generates 1000 cache misses.
        let mut hist = hdrhistogram::Histogram::<u64>::new(3).unwrap();
        let count = match cache_miss {
            true => self.num_cache_misses,
            false => self.num_cache_hits,
        };
        let query_type = if cache_miss { "misses" } else { "hits" };
        let results_data = results.entry(query_type, Unit::Milliseconds, MetricGoal::Decreasing);
        for _ in 0..count {
            let query = if cache_miss {
                gen.generate_cache_miss()?
            } else {
                gen.generate_cache_hit()?
            };
            let start = Instant::now();
            conn.execute(&query.prep, query.params).await?;
            let elapsed = start.elapsed();
            results_data.push(elapsed.as_millis() as f64);
            hist.record(u64::try_from(elapsed.as_micros()).unwrap())
                .unwrap();

            let histogram_name = format!(
                "cache_hit_benchmark.{}_duration",
                if cache_miss { "miss" } else { "hit" }
            );
            benchmark_histogram!(
                &histogram_name,
                Microseconds,
                "Duration of queries executed".into(),
                elapsed.as_micros() as f64
            );
        }

        Ok(())
    }
}
