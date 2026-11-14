use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Edge {
    #[serde(rename = "from")]
    pub from: String,
    pub to: String,
    pub weight: f64,
}

#[derive(Clone, Debug)]
pub struct Graph {
    pub directed: bool,
    pub nodes: HashMap<String, Vec<String>>,
    pub edges: Vec<Edge>,
    lookup: HashMap<(String, String), usize>,
}

impl Default for Graph {
    fn default() -> Self {
        Self::new(false)
    }
}

impl Graph {
    pub fn new(directed: bool) -> Self {
        Self {
            directed,
            nodes: HashMap::new(),
            edges: Vec::new(),
            lookup: HashMap::new(),
        }
    }

    pub fn add_node(&mut self, node: String) {
        self.nodes.entry(node).or_default();
    }

    pub fn add_edge(&mut self, from: String, to: String, weight: f64) {
        if !self.nodes.contains_key(&from) || !self.nodes.contains_key(&to) {
            return;
        }
        if from == to {
            return;
        }
        let weight = if weight.is_finite() { weight } else { 1.0 };
        if let Some(index) = self.edge_index(&from, &to) {
            if weight > self.edges[index].weight {
                self.edges[index].weight = weight;
            }
            return;
        }

        self.nodes
            .get_mut(&from)
            .expect("from node exists")
            .push(to.clone());
        if !self.directed {
            self.nodes
                .get_mut(&to)
                .expect("to node exists")
                .push(from.clone());
        }

        let index = self.edges.len();
        self.edges.push(Edge {
            from: from.clone(),
            to: to.clone(),
            weight,
        });
        self.lookup.insert((from.clone(), to.clone()), index);
        if !self.directed {
            self.lookup.insert((to, from), index);
        }
    }

    fn edge_index(&self, from: &str, to: &str) -> Option<usize> {
        self.lookup
            .get(&(from.to_string(), to.to_string()))
            .copied()
    }

    pub fn has_node(&self, node: &str) -> bool {
        self.nodes.contains_key(node)
    }

    pub fn get_neighbors(&self, node: &str) -> &[String] {
        self.nodes.get(node).map(Vec::as_slice).unwrap_or_default()
    }

    pub fn get_edge_weight(&self, from: &str, to: &str) -> Option<f64> {
        self.lookup
            .get(&(from.to_string(), to.to_string()))
            .map(|index| self.edges[*index].weight)
    }

    pub fn remove_node(&mut self, node: &str) {
        if !self.nodes.contains_key(node) {
            return;
        }
        self.edges
            .retain(|edge| edge.from != node && edge.to != node);
        self.rebuild_lookup();
        self.nodes.remove(node);
        for neighbors in self.nodes.values_mut() {
            neighbors.retain(|neighbor| neighbor != node);
        }
    }

    fn rebuild_lookup(&mut self) {
        self.lookup.clear();
        for (index, edge) in self.edges.iter().enumerate() {
            self.lookup
                .insert((edge.from.clone(), edge.to.clone()), index);
            if !self.directed {
                self.lookup
                    .insert((edge.to.clone(), edge.from.clone()), index);
            }
        }
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    pub fn replace(&mut self, nodes: impl IntoIterator<Item = String>, edges: Vec<Edge>) {
        self.directed = false;
        self.nodes.clear();
        self.edges.clear();
        self.lookup.clear();
        for node in nodes {
            self.nodes.insert(node, Vec::new());
        }
        for edge in edges {
            self.add_edge(edge.from, edge.to, edge.weight);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undirected_edges_work_in_both_directions() {
        let mut graph = Graph::new(false);
        graph.add_node("a".to_string());
        graph.add_node("b".to_string());
        graph.add_edge("a".to_string(), "b".to_string(), 0.4);
        graph.add_edge("a".to_string(), "b".to_string(), 0.9);
        assert_eq!(graph.edge_count(), 1);
        assert_eq!(graph.get_edge_weight("a", "b"), Some(0.9));
        assert_eq!(graph.get_edge_weight("b", "a"), Some(0.9));
        assert_eq!(graph.get_neighbors("a"), ["b".to_string()]);
    }

    #[test]
    fn removing_a_node_drops_incident_edges() {
        let mut graph = Graph::new(false);
        for node in ["a", "b", "c"] {
            graph.add_node(node.to_string());
        }
        graph.add_edge("a".to_string(), "b".to_string(), 0.5);
        graph.add_edge("b".to_string(), "c".to_string(), 0.5);
        graph.remove_node("b");
        assert!(!graph.has_node("b"));
        assert_eq!(graph.edge_count(), 0);
        assert!(graph.get_neighbors("a").is_empty());
        assert!(graph.get_neighbors("c").is_empty());
    }
}
