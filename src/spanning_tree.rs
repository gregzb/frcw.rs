//! Functions for generating random spanning trees.
use crate::buffers::SpanningTreeBuffer;
use crate::graph::{Edge, Graph};
use rand::rngs::SmallRng;
use rand::Rng;
use std::cmp::{max, min};

pub trait SpanningTreeSampler {
    /// Samples a random tree of `graph` using `rng`; inserts the tree into `buf`.
    fn random_spanning_tree(
        &mut self,
        graph: &Graph,
        buf: &mut SpanningTreeBuffer,
        rng: &mut SmallRng,
    );
}
pub use crate::spanning_tree::rmst::RMSTSampler;
pub use crate::spanning_tree::ust::USTSampler;

/// Spanning tree sampling from the uniform distribution.
mod ust {
    use super::*;
    use crate::buffers::RandomRangeBuffer;

    /// A reusable buffer for Wilson's algorithm.
    pub struct USTBuffer {
        /// Boolean representation of the subset of nodes in the spanning tree.
        pub in_tree: Vec<bool>,
        /// The next node in the spanning tree (for a chosen ordering).
        pub next: Vec<i64>,
        /// The edges in the MST.
        pub edges: Vec<usize>,
    }

    impl USTBuffer {
        /// Creates a buffer for a spanning tree of a subgraph
        /// within a graph of size `n`.
        pub fn new(n: usize) -> USTBuffer {
            return USTBuffer {
                in_tree: vec![false; n],
                next: vec![-1 as i64; n],
                edges: Vec::<usize>::with_capacity(n - 1),
            };
        }

        /// Resets the buffer.
        pub fn clear(&mut self) {
            self.in_tree.fill(false);
            self.next.fill(-1);
            self.edges.clear();
        }
    }

    /// Samples random spanning trees from the uniform distribution.
    pub struct USTSampler {
        /// A buffer for Wilson's algorithm.
        ust_buf: USTBuffer,
        /// A reservoir of random bytes (used for quickly selecting random node neighbors).
        range_buf: RandomRangeBuffer,
    }

    impl USTSampler {
        /// Creates a UST sampler (and underlying buffers) for a graph of approximate
        /// size `n`. (A reservoir of random bytes is initialized using `rng`.)
        pub fn new(n: usize, rng: &mut SmallRng) -> USTSampler {
            USTSampler {
                ust_buf: USTBuffer::new(n),
                range_buf: RandomRangeBuffer::new(rng),
            }
        }
    }

    impl SpanningTreeSampler for USTSampler {
        /// Draws a random spanning tree of a graph from the uniform distribution.
        /// Returns nothing; The MST buffer `buf` is updated in place.
        ///
        /// We use Wilson's algorithm [1] (which is, in essence, a self-avoiding random
        /// walk) to generate the tree.
        ///
        /// # Arguments
        /// * `graph` - The graph to form a spanning tree from. The maximum degree
        ///   of the graph must be ≤256; otherwise, sampling from the uniform
        ///   distribution is not guaranteed.
        /// * `buf` - The buffer to insert the spanning tree into.
        /// * `rng` - A random number generator (used to select the spanning tree
        ///   root and refresh the random byte reservoir).
        ///
        /// # References
        /// [1]  Wilson, David Bruce. "Generating random spanning trees more quickly
        ///      than the cover time." Proceedings of the twenty-eighth annual ACM
        ///      symposium on Theory of computing. 1996.
        fn random_spanning_tree(
            &mut self,
            graph: &Graph,
            buf: &mut SpanningTreeBuffer,
            rng: &mut SmallRng,
        ) {
            buf.clear();
            self.ust_buf.clear();
            let n = graph.pops.len();
            let root = rng.gen_range(0..n);
            self.ust_buf.in_tree[root] = true;
            for i in 0..n {
                let mut u = i;
                while !self.ust_buf.in_tree[u] {
                    let neighbors = &graph.neighbors[u];
                    let neighbor =
                        neighbors[self.range_buf.range(rng, neighbors.len() as u8) as usize];
                    self.ust_buf.next[u] = neighbor as i64;
                    u = neighbor;
                }
                u = i;
                while !self.ust_buf.in_tree[u] {
                    self.ust_buf.in_tree[u] = true;
                    u = self.ust_buf.next[u] as usize;
                }
            }

            for (curr, &prev) in self.ust_buf.next.iter().enumerate() {
                if prev >= 0 {
                    let a = min(curr, prev as usize);
                    let b = max(curr, prev as usize);
                    let mut edge_idx = graph.edges_start[a];
                    while graph.edges[edge_idx].0 == a {
                        if graph.edges[edge_idx].1 == b {
                            self.ust_buf.edges.push(edge_idx);
                            break;
                        }
                        edge_idx += 1;
                    }
                }
            }
            if self.ust_buf.edges.len() != n - 1 {
                panic!(
                    "expected to have {} edges in MST but got {}",
                    n - 1,
                    self.ust_buf.edges.len()
                );
            }

            for &edge in self.ust_buf.edges.iter() {
                let Edge(src, dst) = graph.edges[edge];
                buf.st[src].push(dst);
                buf.st[dst].push(src);
            }
        }
    }
}

/// Spanning tree sampling via random edge weights.
mod rmst {
    use super::*;
    use ena::unify::{InPlace, UnificationTable, UnifyKey};

    type EdgeWeight = u32;

    /// Samples random spanning trees by sampling random edge weights and finding
    /// the minimum spanning tree.
    pub struct RMSTSampler {
        /// Buffer for edge weights.
        weights: Vec<EdgeWeight>,
    }

    impl RMSTSampler {
        /// Initializes a random MST sampler for a graph with approximate size `n`.
        pub fn new(n: usize) -> RMSTSampler {
            RMSTSampler {
                weights: Vec::<EdgeWeight>::with_capacity(8 * n),
            }
        }
    }

    // see `ena` UnifyKey example: https://github.com/rust-lang/ena/blob/
    // 94952db24c1ad75c541b65bcb13d57b2cea8c143/src/unify/tests.rs#L245
    #[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
    struct NodeKey(u32);

    impl UnifyKey for NodeKey {
        type Value = ();
        fn index(&self) -> u32 {
            self.0
        }
        fn from_index(u: u32) -> NodeKey {
            NodeKey(u)
        }
        fn tag() -> &'static str {
            "NodeKey"
        }
    }

    /// Given `weights`, finds the minimum spanning tree of `graph` using
    /// Kruskal's algorithm and inserts the tree into `buf`.
    fn minimum_spanning_tree(
        graph: &Graph,
        buf: &mut SpanningTreeBuffer,
        weights: &Vec<EdgeWeight>,
    ) {
        buf.clear();

        // Initialize a union-find data structure to keep track of connected
        // components of the graph.
        // TODO: buffer this?
        let mut ut: UnificationTable<InPlace<NodeKey>> = UnificationTable::new();
        ut.reserve(graph.edges.len());
        let keys: Vec<NodeKey> = graph.edges.iter().map(|_| ut.new_key(())).collect();

        // Apply Kruskal's algorithm: add edges until the graph is connected.
        let mut edges_by_weight = weights
            .iter()
            .enumerate()
            .map(|(idx, &w)| (w, graph.edges[idx]))
            .collect::<Vec<(EdgeWeight, Edge)>>();
        edges_by_weight.sort();

        let n_edges = graph.neighbors.len() - 1;
        let mut unions = 0;
        for (_, Edge(src, dst)) in edges_by_weight.into_iter() {
            if unions == n_edges {
                break;
            }
            if !ut.unioned(keys[src], keys[dst]) {
                ut.union(keys[src], keys[dst]);
                buf.st[src].push(dst);
                buf.st[dst].push(src);
                unions += 1;
            }
        }
        if unions != n_edges {
            panic!(
                "expected to have {} edges in MST but got {}",
                n_edges, unions
            );
        }
    }

    impl SpanningTreeSampler for RMSTSampler {
        /// Draws a random spanning tree of a graph by sampling random edge weights
        /// and finding the minimum spanning tree (using Kruskal's algorithm).
        /// Returns nothing; The MST buffer `buf` is updated in place.
        ///
        /// # Arguments
        /// * `graph` - The graph to form a spanning tree from.
        /// * `buf` - The buffer to insert the spanning tree into.
        /// * `rng` - A random number generator (used to generate random edge weights).
        fn random_spanning_tree(
            &mut self,
            graph: &Graph,
            buf: &mut SpanningTreeBuffer,
            rng: &mut SmallRng,
        ) {
            // Sample edge weights uniformly at random and find the associated MST.
            self.weights.reserve(graph.edges.len());
            rng.fill(&mut self.weights[0..graph.edges.len()]);
            minimum_spanning_tree(graph, buf, &self.weights);
            self.weights.clear();
        }
    }
}
