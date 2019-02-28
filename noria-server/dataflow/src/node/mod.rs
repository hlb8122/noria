use domain;
use ops;
use petgraph;
use prelude::*;
use std::collections::{HashMap, HashSet};
use std::ops::{Deref, DerefMut};

mod process;
#[cfg(test)]
crate use self::process::materialize;

pub mod special;
pub use self::special::StreamUpdate;

mod ntype;
crate use self::ntype::NodeType; // crate viz for tests

mod replica;
pub use self::replica::ReplicaType;

mod debug;

// NOTE(jfrg): the migration code should probably move into the dataflow crate...
// it is the reason why so much stuff here is pub

#[derive(Clone, Serialize, Deserialize)]
pub struct Node {
    name: String,
    index: Option<IndexPair>,
    domain: Option<domain::Index>,

    fields: Vec<String>,
    children: Vec<LocalNodeIndex>,
    inner: NodeType,
    taken: bool,

    sharded_by: Sharding,
    replica: Option<ReplicaType>,
}

// constructors
impl Node {
    pub fn new<S1, FS, S2, NT>(name: S1, fields: FS, inner: NT) -> Node
    where
        S1: ToString,
        S2: ToString,
        FS: IntoIterator<Item = S2>,
        NT: Into<NodeType>,
    {
        Node {
            name: name.to_string(),
            index: None,
            domain: None,

            fields: fields.into_iter().map(|s| s.to_string()).collect(),
            children: Vec::new(),
            inner: inner.into(),
            taken: false,

            sharded_by: Sharding::None,
            replica: None,
        }
    }

    fn new_with_replica<S1, FS, S2, NT>(
        name: S1,
        fields: FS,
        inner: NT,
        replica: &Option<ReplicaType>,
    ) -> Node
    where
        S1: ToString,
        S2: ToString,
        FS: IntoIterator<Item = S2>,
        NT: Into<NodeType>,
    {
        let mut n = Self::new(name, fields, inner);
        n.replica = replica.clone();
        n
    }

    pub fn mirror<NT: Into<NodeType>>(&self, n: NT) -> Node {
        Self::new_with_replica(&*self.name, &self.fields, n, &self.replica)
    }

    pub fn named_mirror<NT: Into<NodeType>>(&self, n: NT, name: String) -> Node {
        Self::new_with_replica(name, &self.fields, n, &self.replica)
    }
}

#[must_use]
pub struct DanglingDomainNode(Node);

impl DanglingDomainNode {
    pub fn finalize(self, graph: &Graph) -> Node {
        let mut n = self.0;
        let ni = n.global_addr();
        let dm = n.domain();
        n.children = graph
            .neighbors_directed(ni, petgraph::EdgeDirection::Outgoing)
            .filter(|&c| graph[c].domain() == dm)
            .map(|ni| graph[ni].local_addr())
            .collect();

        n
    }
}

// expternal parts of Ingredient
impl Node {
    /// Called when a node is first connected to the graph.
    ///
    /// All its ancestors are present, but this node and its children may not have been connected
    /// yet.
    pub fn on_connected(&mut self, graph: &Graph) {
        Ingredient::on_connected(&mut **self, graph)
    }

    pub fn on_commit(&mut self, remap: &HashMap<NodeIndex, IndexPair>) {
        // this is *only* overwritten for these asserts.
        assert!(!self.taken);
        if let NodeType::Internal(ref mut i) = self.inner {
            i.on_commit(self.index.unwrap().as_global(), remap)
        }
    }

    /// May return a set of nodes such that *one* of the given ancestors *must* be the one to be
    /// replayed if this node's state is to be initialized.
    pub fn must_replay_among(&self) -> Option<HashSet<NodeIndex>> {
        Ingredient::must_replay_among(&**self)
    }

    /// Translate a column in this ingredient into the corresponding column(s) in
    /// parent ingredients. None for the column means that the parent doesn't
    /// have an associated column. Similar to resolve, but does not depend on
    /// materialization, and returns results even for computed columns.
    pub fn parent_columns(&self, column: usize) -> Vec<(NodeIndex, Option<usize>)> {
        Ingredient::parent_columns(&**self, column)
    }

    /// Resolve where the given field originates from. If the view is materialized, or the value is
    /// otherwise created by this view, None should be returned.
    pub fn resolve(&self, i: usize) -> Option<Vec<(NodeIndex, usize)>> {
        Ingredient::resolve(&**self, i)
    }

    /// Returns true if this operator requires a full materialization
    pub fn requires_full_materialization(&self) -> bool {
        Ingredient::requires_full_materialization(&**self)
    }

    pub fn can_query_through(&self) -> bool {
        Ingredient::can_query_through(&**self)
    }

    pub fn is_join(&self) -> bool {
        Ingredient::is_join(&**self)
    }

    pub fn ancestors(&self) -> Vec<NodeIndex> {
        Ingredient::ancestors(&**self)
    }

    /// Produce a compact, human-readable description of this node for Graphviz.
    ///
    /// If `detailed` is true, emit more info.
    ///
    ///  Symbol   Description
    /// --------|-------------
    ///    B    |  Base
    ///    ||   |  Concat
    ///    ⧖    |  Latest
    ///    γ    |  Group by
    ///   |*|   |  Count
    ///    𝛴    |  Sum
    ///    ⋈    |  Join
    ///    ⋉    |  Left join
    ///    ⋃    |  Union
    pub fn description(&self, detailed: bool) -> String {
        Ingredient::description(&**self, detailed)
    }
}

// publicly accessible attributes
impl Node {
    pub fn name(&self) -> &str {
        &*self.name
    }

    pub fn fields(&self) -> &[String] {
        &self.fields[..]
    }

    pub fn sharded_by(&self) -> Sharding {
        self.sharded_by
    }

    /// Set this node's sharding property.
    pub fn shard_by(&mut self, s: Sharding) {
        self.sharded_by = s;
    }
}

// events
impl Node {
    pub fn take(&mut self) -> DanglingDomainNode {
        assert!(!self.taken);
        assert!(
            (!self.is_internal() && !self.is_base()) || self.domain.is_some(),
            "tried to take unassigned node"
        );

        let inner = self.inner.take();
        let mut n = self.mirror(inner);
        n.index = self.index;
        n.domain = self.domain;
        self.taken = true;

        DanglingDomainNode(n)
    }

    pub fn remove(&mut self) {
        self.inner = NodeType::Dropped;
    }
}

// derefs
impl Node {
    crate fn with_sharder_mut<F>(&mut self, f: F)
    where
        F: FnOnce(&mut special::Sharder),
    {
        match self.inner {
            NodeType::Sharder(ref mut s) => f(s),
            _ => unreachable!(),
        }
    }

    pub fn with_sharder<'a, F, R>(&'a self, f: F) -> Option<R>
    where
        F: FnOnce(&'a special::Sharder) -> R,
        R: 'a,
    {
        match self.inner {
            NodeType::Sharder(ref s) => Some(f(s)),
            _ => None,
        }
    }

    crate fn with_egress_mut<F>(&mut self, f: F)
    where
        F: FnOnce(&mut special::Egress),
    {
        match self.inner {
            NodeType::Egress(Some(ref mut e)) => f(e),
            _ => unreachable!(),
        }
    }

    pub fn with_ingress_mut<'a, F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut special::Ingress) -> R,
    {
        match self.inner {
            NodeType::Ingress(ref mut i) => f(i),
            _ => unreachable!(),
        }
    }

    pub fn with_reader_mut<'a, F, R>(&'a mut self, f: F) -> Result<R, ()>
    where
        F: FnOnce(&'a mut special::Reader) -> R,
        R: 'a,
    {
        match self.inner {
            NodeType::Reader(ref mut r) => Ok(f(r)),
            _ => Err(()),
        }
    }

    pub fn with_reader<'a, F, R>(&'a self, f: F) -> Result<R, ()>
    where
        F: FnOnce(&'a special::Reader) -> R,
        R: 'a,
    {
        match self.inner {
            NodeType::Reader(ref r) => Ok(f(r)),
            _ => Err(()),
        }
    }

    pub fn get_base(&self) -> Option<&special::Base> {
        if let NodeType::Base(ref b) = self.inner {
            Some(b)
        } else {
            None
        }
    }

    pub fn suggest_indexes(&self, n: NodeIndex) -> HashMap<NodeIndex, (Vec<usize>, bool)> {
        match self.inner {
            NodeType::Internal(ref i) => i.suggest_indexes(n),
            NodeType::Base(ref b) => b.suggest_indexes(n),
            _ => HashMap::new(),
        }
    }
}

impl Deref for Node {
    type Target = ops::NodeOperator;
    fn deref(&self) -> &Self::Target {
        match self.inner {
            NodeType::Internal(ref i) => i,
            _ => unreachable!(),
        }
    }
}

impl DerefMut for Node {
    fn deref_mut(&mut self) -> &mut Self::Target {
        assert!(!self.taken);
        match self.inner {
            NodeType::Internal(ref mut i) => i,
            _ => unreachable!(),
        }
    }
}

// children
impl Node {
    crate fn child(&self, i: usize) -> &LocalNodeIndex {
        &self.children[i]
    }

    crate fn nchildren(&self) -> usize {
        self.children.len()
    }
}

// attributes
impl Node {
    crate fn add_child(&mut self, child: LocalNodeIndex) {
        self.children.push(child);
    }

    crate fn try_remove_child(&mut self, child: LocalNodeIndex) -> bool {
        for i in 0..self.children.len() {
            if self.children[i] == child {
                self.children.swap_remove(i);
                return true;
            }
        }
        false
    }

    pub fn add_column(&mut self, field: &str) -> usize {
        self.fields.push(field.to_string());
        self.fields.len() - 1
    }

    pub fn has_domain(&self) -> bool {
        self.domain.is_some()
    }

    pub fn domain(&self) -> domain::Index {
        match self.domain {
            Some(domain) => domain,
            None => {
                unreachable!(
                    "asked for unset domain for {:?} {}",
                    self,
                    self.global_addr().index()
                );
            }
        }
    }

    pub fn local_addr(&self) -> LocalNodeIndex {
        match self.index {
            Some(idx) if idx.has_local() => *idx,
            Some(_) | None => unreachable!("asked for unset addr for {:?}", self),
        }
    }

    pub fn global_addr(&self) -> NodeIndex {
        match self.index {
            Some(ref index) => index.as_global(),
            None => {
                unreachable!("asked for unset index for {:?}", self);
            }
        }
    }

    pub fn get_base_mut(&mut self) -> Option<&mut special::Base> {
        if let NodeType::Base(ref mut b) = self.inner {
            Some(b)
        } else {
            None
        }
    }

    pub fn add_to(&mut self, domain: domain::Index) {
        assert_eq!(self.domain, None);
        assert!(!self.is_dropped());
        self.domain = Some(domain);
    }

    pub fn set_finalized_addr(&mut self, addr: IndexPair) {
        self.index = Some(addr);
    }
}

// replication
impl Node {
    pub fn replica_type(&self) -> Option<ReplicaType> {
        self.replica.clone()
    }

    pub fn set_replica_type(&mut self, rt: ReplicaType) {
        self.replica = Some(rt);
    }

    pub fn remove_replica_type(&mut self) {
        assert!(self.replica.is_some());
        self.replica = None;
    }

    pub fn into_full(&mut self) {
        let op = match **self {
            NodeOperator::Replica(ref mut r) => r.take_op(),
            _ => unreachable!(),
        };
        self.inner = NodeType::Internal(*op);
    }

    pub fn recover(&mut self, new_domain: domain::Index) {
        assert!(self.domain.is_some());
        assert!(!self.is_dropped());
        self.domain = Some(new_domain);
        self.taken = false;

        if let NodeType::Egress(None) = self.inner {
            let e = self::special::Egress::default();
            self.inner = NodeType::Egress(Some(e));
        }
    }
}

// is this or that?
impl Node {
    pub fn is_dropped(&self) -> bool {
        if let NodeType::Dropped = self.inner {
            true
        } else {
            false
        }
    }

    pub fn is_egress(&self) -> bool {
        if let NodeType::Egress { .. } = self.inner {
            true
        } else {
            false
        }
    }

    pub fn is_reader(&self) -> bool {
        if let NodeType::Reader { .. } = self.inner {
            true
        } else {
            false
        }
    }

    pub fn is_ingress(&self) -> bool {
        if let NodeType::Ingress(..) = self.inner {
            true
        } else {
            false
        }
    }

    pub fn is_sender(&self) -> bool {
        match self.inner {
            NodeType::Egress { .. } | NodeType::Sharder(..) => true,
            _ => false,
        }
    }

    pub fn is_internal(&self) -> bool {
        if let NodeType::Internal(..) = self.inner {
            true
        } else {
            false
        }
    }

    pub fn is_source(&self) -> bool {
        if let NodeType::Source { .. } = self.inner {
            true
        } else {
            false
        }
    }

    pub fn is_sharder(&self) -> bool {
        if let NodeType::Sharder { .. } = self.inner {
            true
        } else {
            false
        }
    }

    pub fn is_base(&self) -> bool {
        if let NodeType::Base(..) = self.inner {
            true
        } else {
            false
        }
    }

    pub fn is_shard_merger(&self) -> bool {
        if let NodeType::Internal(NodeOperator::Union(ref u)) = self.inner {
            u.is_shard_merger()
        } else {
            false
        }
    }
}