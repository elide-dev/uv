//! This module implements marker tree operations using Algebraic Decision Diagrams (ADD).
//!
//! An ADD is a tree of decision nodes as well as two terminal nodes, `true` and `false`. Marker
//! variables are represented as decision nodes. The edge from a decision node to it's child
//! represents a particular assignment of a value to that variable. Depending on the type of
//! variable, an edge can be represented by binary values or a disjoint set of ranges, as opposed
//! to a traditional Binary Decision Diagram.
//!
//! For example, the marker `python_version > '3.7' and os_name == 'Linux'` creates the following
//! marker tree:
//!
//! ```text
//! python_version:
//!   (> '3.7')  -> os_name:
//!                   (> 'Linux')  -> FALSE
//!                   (== 'Linux') -> TRUE
//!                   (< 'Linux')  -> FALSE
//!   (<= '3.7') -> FALSE
//! ```
//!
//! Specifically, a marker tree is represented as a Reduced Ordered ADD. An ADD is ordered if
//! different variables appear in the same order on all paths from the root. Additionally, an ADD
//! is reduced if:
//! - Isomorphic nodes are merged.
//! - Nodes with isomorphic children are eliminated.
//!
//! These two rules provide an important guarantee for marker trees: marker trees are canonical for
//! a given marker function and variable ordering. Because variable ordering is defined at compile-time,
//! this means any functionally equivalent marker trees are normalized upon construction. Importantly,
//! this means that we can identify trivially true marker trees, as well as unsatisfiable marker trees.
//! This provides important information to the resolver when forking.
//!
//! ADDs provide polynomial time operations such as conjunction and negation, which is important as marker
//! trees are combined during universal resolution. Because ADDs solve the SAT problem, constructing an
//! arbitrary ADD can theoretically take exponential time in the worst case. However, in practice, marker trees
//! have a limited number of variables and user-provided marker trees are typically very simple.
//!
//! Additionally, the implementation in this module uses complemented edges, meaning a marker tree and
//! it's complement are represented by the same node internally. This allows cheap constant-time marker
//! tree negation. It also allows us to only implement a single operation for both `AND` and `OR`, implementing
//! the other in terms of its De Morgan Complement.
//!
//! ADDs are created and managed through the global [`Interner`]. A given ADD is referenced through
//! a [`NodeId`], which represents a potentially complemented reference to a [`Node`] in the interner,
//! or a terminal `true`/`false` node. Interning allows the reduction rule that isomorphic nodes are
//! merged to be applied globally.

use std::cmp::Ordering;
use std::fmt;
use std::ops::Bound;
use std::sync::{LazyLock, Mutex, MutexGuard};

use arcstr::ArcStr;
use itertools::{Either, Itertools};
use rustc_hash::FxHashMap;
use version_ranges::Ranges;

use uv_pep440::{Operator, Version, VersionSpecifier, release_specifier_to_range};

use crate::marker::MarkerValueExtra;
use crate::marker::lowering::{
    CanonicalMarkerListPair, CanonicalMarkerValueExtra, CanonicalMarkerValueString,
    CanonicalMarkerValueVersion,
};
use crate::marker::tree::ContainerOperator;
use crate::{
    ExtraOperator, MarkerExpression, MarkerOperator, MarkerValueString, MarkerValueVersion,
};

/// The global node interner.
pub(crate) static INTERNER: LazyLock<Interner> = LazyLock::new(Interner::default);

/// An interner for decision nodes.
///
/// Interning decision nodes allows isomorphic nodes to be automatically merged.
/// It also allows nodes to cheaply compared.
#[derive(Default)]
pub(crate) struct Interner {
    pub(crate) shared: InternerShared,
    state: Mutex<InternerState>,
}

/// The shared part of an [`Interner`], which can be accessed without a lock.
#[derive(Default)]
pub(crate) struct InternerShared {
    /// A list of unique [`Node`]s.
    nodes: boxcar::Vec<Node>,
}

/// The mutable [`Interner`] state, stored behind a lock.
#[derive(Default)]
struct InternerState {
    /// A map from a [`Node`] to a unique [`NodeId`], representing an index
    /// into [`InternerShared`].
    unique: FxHashMap<Node, NodeId>,

    /// A cache for `AND` operations between two nodes.
    /// Note that `OR` is implemented in terms of `AND`.
    cache: FxHashMap<(NodeId, NodeId), NodeId>,

    /// The [`NodeId`] for the disjunction of known, mutually incompatible markers.
    exclusions: Option<NodeId>,
}

impl InternerShared {
    /// Returns the node for the given [`NodeId`].
    pub(crate) fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id.index()]
    }
}

impl Interner {
    /// Locks the interner state, returning a guard that can be used to perform marker
    /// operations.
    pub(crate) fn lock(&self) -> InternerGuard<'_> {
        InternerGuard {
            state: self.state.lock().unwrap(),
            shared: &self.shared,
        }
    }
}

/// A lock of [`InternerState`].
pub(crate) struct InternerGuard<'a> {
    state: MutexGuard<'a, InternerState>,
    shared: &'a InternerShared,
}

impl InternerGuard<'_> {
    /// Creates a decision node with the given variable and children.
    fn create_node(&mut self, var: Variable, children: Edges) -> NodeId {
        let mut node = Node { var, children };
        let mut first = node.children.nodes().next().unwrap();

        // With a complemented edge representation, there are two ways to represent the same node:
        // complementing the root and all children edges results in the same node. To ensure markers
        // are canonical, the first child edge is never complemented.
        let mut flipped = false;
        if first.is_complement() {
            node = node.not();
            first = first.not();
            flipped = true;
        }

        // Reduction: If all children refer to the same node, we eliminate the parent node
        // and just return the child.
        if node.children.nodes().all(|node| node == first) {
            return if flipped { first.not() } else { first };
        }

        // Insert the node.
        let id = self
            .state
            .unique
            .entry(node.clone())
            .or_insert_with(|| NodeId::new(self.shared.nodes.push(node), false));

        if flipped { id.not() } else { *id }
    }

    /// Returns a decision node for a single marker expression.
    pub(crate) fn expression(&mut self, expr: MarkerExpression) -> NodeId {
        let (var, children) = match expr {
            // A variable representing the output of a version key. Edges correspond
            // to disjoint version ranges.
            MarkerExpression::Version { key, specifier } => match key {
                MarkerValueVersion::ImplementationVersion => (
                    Variable::Version(CanonicalMarkerValueVersion::ImplementationVersion),
                    Edges::from_specifier(specifier),
                ),
                MarkerValueVersion::PythonFullVersion => (
                    Variable::Version(CanonicalMarkerValueVersion::PythonFullVersion),
                    Edges::from_specifier(specifier),
                ),
                // Normalize `python_version` markers to `python_full_version` nodes.
                MarkerValueVersion::PythonVersion => {
                    match python_version_to_full_version(specifier.only_release()) {
                        Ok(specifier) => (
                            Variable::Version(CanonicalMarkerValueVersion::PythonFullVersion),
                            Edges::from_specifier(specifier),
                        ),
                        Err(node) => return node,
                    }
                }
            },
            // A variable representing the output of a version key. Edges correspond
            // to disjoint version ranges.
            MarkerExpression::VersionIn {
                key,
                versions,
                operator,
            } => match key {
                MarkerValueVersion::ImplementationVersion => (
                    Variable::Version(CanonicalMarkerValueVersion::ImplementationVersion),
                    Edges::from_versions(&versions, operator),
                ),
                MarkerValueVersion::PythonFullVersion => (
                    Variable::Version(CanonicalMarkerValueVersion::PythonFullVersion),
                    Edges::from_versions(&versions, operator),
                ),
                // Normalize `python_version` markers to `python_full_version` nodes.
                MarkerValueVersion::PythonVersion => {
                    match Edges::from_python_versions(versions, operator) {
                        Ok(edges) => (
                            Variable::Version(CanonicalMarkerValueVersion::PythonFullVersion),
                            edges,
                        ),
                        Err(node) => return node,
                    }
                }
            },
            // The `in` and `contains` operators are a bit different than other operators.
            // In particular, they do not represent a particular value for the corresponding
            // variable, and can overlap. For example, `'nux' in os_name` and `os_name == 'Linux'`
            // can both be `true` in the same marker environment, and so cannot be represented by
            // the same variable. Because of this, we represent `in` and `contains`, as well as
            // their negations, as distinct variables, unrelated to the range of a given key.
            //
            // Note that in the presence of the `in` operator, we may not be able to simplify
            // some marker trees to a constant `true` or `false`. For example, it is not trivial to
            // detect that `os_name > 'z' and os_name in 'Linux'` is unsatisfiable.
            MarkerExpression::String {
                key,
                operator: MarkerOperator::In,
                value,
            } => (
                Variable::In {
                    key: key.into(),
                    value,
                },
                Edges::from_bool(true),
            ),
            MarkerExpression::String {
                key,
                operator: MarkerOperator::NotIn,
                value,
            } => (
                Variable::In {
                    key: key.into(),
                    value,
                },
                Edges::from_bool(false),
            ),
            MarkerExpression::String {
                key,
                operator: MarkerOperator::Contains,
                value,
            } => (
                Variable::Contains {
                    key: key.into(),
                    value,
                },
                Edges::from_bool(true),
            ),
            MarkerExpression::String {
                key,
                operator: MarkerOperator::NotContains,
                value,
            } => (
                Variable::Contains {
                    key: key.into(),
                    value,
                },
                Edges::from_bool(false),
            ),
            // A variable representing the output of a string key. Edges correspond
            // to disjoint string ranges.
            MarkerExpression::String {
                key,
                operator,
                value,
            } => {
                // Normalize `platform_system` markers to `sys_platform` nodes.
                //
                // The `platform` module is "primarily intended for diagnostic information to be
                // read by humans."
                //
                // We only normalize when we can confidently guarantee that the values are
                // exactly equivalent. For example, we normalize `platform_system == 'Windows'`
                // to `sys_platform == 'win32'`, but we do not normalize `platform_system == 'FreeBSD'`
                // to `sys_platform == 'freebsd'`, since FreeBSD typically includes a major version
                // in its `sys.platform` output.
                //
                // For cases that aren't normalized, we do our best to encode known-incompatible
                // values in `exclusions`.
                //
                // See: https://discuss.python.org/t/clarify-usage-of-platform-system/70900
                let (key, value) = match (key, value.as_ref()) {
                    (MarkerValueString::PlatformSystem, "Windows") => (
                        CanonicalMarkerValueString::SysPlatform,
                        arcstr::literal!("win32"),
                    ),
                    (MarkerValueString::PlatformSystem, "Darwin") => (
                        CanonicalMarkerValueString::SysPlatform,
                        arcstr::literal!("darwin"),
                    ),
                    (MarkerValueString::PlatformSystem, "Linux") => (
                        CanonicalMarkerValueString::SysPlatform,
                        arcstr::literal!("linux"),
                    ),
                    (MarkerValueString::PlatformSystem, "AIX") => (
                        CanonicalMarkerValueString::SysPlatform,
                        arcstr::literal!("aix"),
                    ),
                    (MarkerValueString::PlatformSystem, "Emscripten") => (
                        CanonicalMarkerValueString::SysPlatform,
                        arcstr::literal!("emscripten"),
                    ),
                    // See: https://peps.python.org/pep-0738/#sys
                    (MarkerValueString::PlatformSystem, "Android") => (
                        CanonicalMarkerValueString::SysPlatform,
                        arcstr::literal!("android"),
                    ),
                    _ => (key.into(), value),
                };
                (Variable::String(key), Edges::from_string(operator, value))
            }
            MarkerExpression::List { pair, operator } => (
                Variable::List(pair),
                Edges::from_bool(operator == ContainerOperator::In),
            ),
            // A variable representing the existence or absence of a particular extra.
            MarkerExpression::Extra {
                name: MarkerValueExtra::Extra(extra),
                operator: ExtraOperator::Equal,
            } => (
                Variable::Extra(CanonicalMarkerValueExtra::Extra(extra)),
                Edges::from_bool(true),
            ),
            MarkerExpression::Extra {
                name: MarkerValueExtra::Extra(extra),
                operator: ExtraOperator::NotEqual,
            } => (
                Variable::Extra(CanonicalMarkerValueExtra::Extra(extra)),
                Edges::from_bool(false),
            ),
            // Invalid `extra` names are always `false`.
            MarkerExpression::Extra {
                name: MarkerValueExtra::Arbitrary(_),
                ..
            } => return NodeId::FALSE,
        };

        self.create_node(var, children)
    }

    /// Returns a decision node representing the disjunction of two nodes.
    pub(crate) fn or(&mut self, xi: NodeId, yi: NodeId) -> NodeId {
        // We take advantage of cheap negation here and implement OR in terms
        // of it's De Morgan complement.
        self.and(xi.not(), yi.not()).not()
    }

    /// Returns a decision node representing the conjunction of two nodes.
    pub(crate) fn and(&mut self, xi: NodeId, yi: NodeId) -> NodeId {
        if xi.is_true() {
            return yi;
        }
        if yi.is_true() {
            return xi;
        }
        if xi == yi {
            return xi;
        }
        if xi.is_false() || yi.is_false() {
            return NodeId::FALSE;
        }
        // `X and not X` is `false` by definition.
        if xi.not() == yi {
            return NodeId::FALSE;
        }

        // The operation was memoized.
        if let Some(result) = self.state.cache.get(&(xi, yi)) {
            return *result;
        }

        let (x, y) = (self.shared.node(xi), self.shared.node(yi));

        // Determine whether the conjunction _could_ contain a conflict.
        //
        // As an optimization, we only have to perform this check at the top-level, since these
        // variables are given higher priority in the tree. In other words, if they're present, they
        // _must_ be at the top; and if they're not at the top, we know they aren't present in any
        // children.
        let conflicts = x.var.is_conflicting_variable() && y.var.is_conflicting_variable();

        // Perform Shannon Expansion of the higher order variable.
        let (func, children) = match x.var.cmp(&y.var) {
            // X is higher order than Y, apply Y to every child of X.
            Ordering::Less => {
                let children = x.children.map(xi, |node| self.and(node, yi));
                (x.var.clone(), children)
            }
            // Y is higher order than X, apply X to every child of Y.
            Ordering::Greater => {
                let children = y.children.map(yi, |node| self.and(node, xi));
                (y.var.clone(), children)
            }
            // X and Y represent the same variable, merge their children.
            Ordering::Equal => {
                let children = x.children.apply(xi, &y.children, yi, |x, y| self.and(x, y));
                (x.var.clone(), children)
            }
        };

        // Create the output node.
        let node = self.create_node(func, children);

        // If the node includes known incompatibilities, map it to `false`.
        let node = if conflicts {
            let exclusions = self.exclusions();
            if self.disjointness(node, exclusions.not()) {
                NodeId::FALSE
            } else {
                node
            }
        } else {
            node
        };

        // Memoize the result of this operation.
        //
        // ADDs often contain duplicated subgraphs in distinct branches due to the restricted
        // variable ordering. Memoizing allows ADD operations to remain polynomial time.
        self.state.cache.insert((xi, yi), node);

        node
    }

    /// Returns `true` if there is no environment in which both marker trees can apply,
    /// i.e. their conjunction is always `false`.
    pub(crate) fn is_disjoint(&mut self, xi: NodeId, yi: NodeId) -> bool {
        // `false` is disjoint with any marker.
        if xi.is_false() || yi.is_false() {
            return true;
        }
        // `true` is not disjoint with any marker except `false`.
        if xi.is_true() || yi.is_true() {
            return false;
        }
        // `X` and `X` are not disjoint.
        if xi == yi {
            return false;
        }
        // `X` and `not X` are disjoint by definition.
        if xi.not() == yi {
            return true;
        }

        let (x, y) = (self.shared.node(xi), self.shared.node(yi));

        // Determine whether the conjunction _could_ contain a conflict.
        //
        // As an optimization, we only have to perform this check at the top-level, since these
        // variables are given higher priority in the tree. In other words, if they're present, they
        // _must_ be at the top; and if they're not at the top, we know they aren't present in any
        // children.
        if x.var.is_conflicting_variable() && y.var.is_conflicting_variable() {
            return self.and(xi, yi).is_false();
        }

        // Perform Shannon Expansion of the higher order variable.
        match x.var.cmp(&y.var) {
            // X is higher order than Y, Y must be disjoint with every child of X.
            Ordering::Less => x
                .children
                .nodes()
                .all(|x| self.disjointness(x.negate(xi), yi)),
            // Y is higher order than X, X must be disjoint with every child of Y.
            Ordering::Greater => y
                .children
                .nodes()
                .all(|y| self.disjointness(y.negate(yi), xi)),
            // X and Y represent the same variable, their merged edges must be unsatisfiable.
            Ordering::Equal => x.children.is_disjoint(xi, &y.children, yi, self),
        }
    }

    /// Returns `true` if there is no environment in which both marker trees can apply,
    /// i.e., their conjunction is always `false`.
    fn disjointness(&mut self, xi: NodeId, yi: NodeId) -> bool {
        // NOTE(charlie): This is equivalent to `is_disjoint`, with the exception that it doesn't
        // perform the mutually-incompatible marker check. If it did, we'd create an infinite loop,
        // since `is_disjoint` calls `and` (when relevant variables are present) which then calls
        // `disjointness`.

        // `false` is disjoint with any marker.
        if xi.is_false() || yi.is_false() {
            return true;
        }
        // `true` is not disjoint with any marker except `false`.
        if xi.is_true() || yi.is_true() {
            return false;
        }
        // `X` and `X` are not disjoint.
        if xi == yi {
            return false;
        }
        // `X` and `not X` are disjoint by definition.
        if xi.not() == yi {
            return true;
        }

        let (x, y) = (self.shared.node(xi), self.shared.node(yi));

        // Perform Shannon Expansion of the higher order variable.
        match x.var.cmp(&y.var) {
            // X is higher order than Y, Y must be disjoint with every child of X.
            Ordering::Less => x
                .children
                .nodes()
                .all(|x| self.disjointness(x.negate(xi), yi)),
            // Y is higher order than X, X must be disjoint with every child of Y.
            Ordering::Greater => y
                .children
                .nodes()
                .all(|y| self.disjointness(y.negate(yi), xi)),
            // X and Y represent the same variable, their merged edges must be unsatisfiable.
            Ordering::Equal => x.children.is_disjoint(xi, &y.children, yi, self),
        }
    }

    // Restrict the output of a given boolean variable in the tree.
    //
    // If the provided function `f` returns a `Some` boolean value, the tree will be simplified
    // with the assumption that the given variable is restricted to that value. If the function
    // returns `None`, the variable will not be affected.
    pub(crate) fn restrict(&mut self, i: NodeId, f: &impl Fn(&Variable) -> Option<bool>) -> NodeId {
        if matches!(i, NodeId::TRUE | NodeId::FALSE) {
            return i;
        }

        let node = self.shared.node(i);
        if let Edges::Boolean { high, low } = node.children {
            if let Some(value) = f(&node.var) {
                // Restrict this variable to the given output by merging it
                // with the relevant child.
                let node = if value { high } else { low };
                return self.restrict(node.negate(i), f);
            }
        }

        // Restrict all nodes recursively.
        let children = node.children.map(i, |node| self.restrict(node, f));
        self.create_node(node.var.clone(), children)
    }

    /// Returns a new tree where the only nodes remaining are non-`extra`
    /// nodes.
    ///
    /// If there are only `extra` nodes, then this returns a tree that is
    /// always true.
    ///
    /// This works by assuming all `extra` nodes are always true.
    ///
    /// For example, given a marker like
    /// `((os_name == ... and extra == foo) or (sys_platform == ... and extra != foo))`,
    /// this would return a marker
    /// `os_name == ... or sys_platform == ...`.
    pub(crate) fn without_extras(&mut self, mut i: NodeId) -> NodeId {
        if matches!(i, NodeId::TRUE | NodeId::FALSE) {
            return i;
        }

        let parent = i;
        let node = self.shared.node(i);
        if matches!(node.var, Variable::Extra(_)) {
            i = NodeId::FALSE;
            for child in node.children.nodes() {
                i = self.or(i, child.negate(parent));
            }
            if i.is_true() {
                return NodeId::TRUE;
            }
            self.without_extras(i)
        } else {
            // Restrict all nodes recursively.
            let children = node.children.map(i, |node| self.without_extras(node));
            self.create_node(node.var.clone(), children)
        }
    }

    /// Returns a new tree where the only nodes remaining are `extra` nodes.
    ///
    /// If there are no extra nodes, then this returns a tree that is always
    /// true.
    ///
    /// This works by assuming all non-`extra` nodes are always true.
    pub(crate) fn only_extras(&mut self, mut i: NodeId) -> NodeId {
        if matches!(i, NodeId::TRUE | NodeId::FALSE) {
            return i;
        }

        let parent = i;
        let node = self.shared.node(i);
        if !matches!(node.var, Variable::Extra(_)) {
            i = NodeId::FALSE;
            for child in node.children.nodes() {
                i = self.or(i, child.negate(parent));
            }
            if i.is_true() {
                return NodeId::TRUE;
            }
            self.only_extras(i)
        } else {
            // Restrict all nodes recursively.
            let children = node.children.map(i, |node| self.only_extras(node));
            self.create_node(node.var.clone(), children)
        }
    }

    /// Simplify this tree by *assuming* that the Python version range provided
    /// is true and that the complement of it is false.
    ///
    /// For example, with `requires-python = '>=3.8'` and a marker tree of
    /// `python_full_version >= '3.8' and python_full_version <= '3.10'`, this
    /// would result in a marker of `python_full_version <= '3.10'`.
    pub(crate) fn simplify_python_versions(
        &mut self,
        i: NodeId,
        py_lower: Bound<&Version>,
        py_upper: Bound<&Version>,
    ) -> NodeId {
        if matches!(i, NodeId::TRUE | NodeId::FALSE)
            || matches!((py_lower, py_upper), (Bound::Unbounded, Bound::Unbounded))
        {
            return i;
        }

        let node = self.shared.node(i);
        // Look for a `python_full_version` expression, otherwise
        // we recursively simplify.
        let Node {
            var: Variable::Version(CanonicalMarkerValueVersion::PythonFullVersion),
            children: Edges::Version { edges },
        } = node
        else {
            // Simplify all nodes recursively.
            let children = node.children.map(i, |node_id| {
                self.simplify_python_versions(node_id, py_lower, py_upper)
            });
            return self.create_node(node.var.clone(), children);
        };
        let py_range = Ranges::from_range_bounds((py_lower.cloned(), py_upper.cloned()));
        if py_range.is_empty() {
            // Oops, the bounds imply there is nothing that can match,
            // so we always evaluate to false.
            return NodeId::FALSE;
        }
        let mut new = SmallVec::new();
        for &(ref range, node) in edges {
            let overlap = range.intersection(&py_range);
            if overlap.is_empty() {
                continue;
            }
            new.push((overlap.clone(), node));
        }

        // Now that we know the only ranges left are those that
        // intersect with our lower/upper Python version bounds, we
        // can "extend" out the lower/upper bounds here all the way to
        // negative and positive infinity, respectively.
        //
        // This has the effect of producing a marker that is only
        // applicable in a context where the Python lower/upper bounds
        // are known to be satisfied.
        let &(ref first_range, first_node_id) = new.first().unwrap();
        let first_upper = first_range.bounding_range().unwrap().1;
        let clipped = Ranges::from_range_bounds((Bound::Unbounded, first_upper.cloned()));
        *new.first_mut().unwrap() = (clipped, first_node_id);

        let &(ref last_range, last_node_id) = new.last().unwrap();
        let last_lower = last_range.bounding_range().unwrap().0;
        let clipped = Ranges::from_range_bounds((last_lower.cloned(), Bound::Unbounded));
        *new.last_mut().unwrap() = (clipped, last_node_id);

        self.create_node(node.var.clone(), Edges::Version { edges: new })
            .negate(i)
    }

    /// Complexify this tree by requiring the given Python version
    /// range to be true in order for this marker tree to evaluate to
    /// true in all circumstances.
    ///
    /// For example, with `requires-python = '>=3.8'` and a marker tree of
    /// `python_full_version <= '3.10'`, this would result in a marker of
    /// `python_full_version >= '3.8' and python_full_version <= '3.10'`.
    pub(crate) fn complexify_python_versions(
        &mut self,
        i: NodeId,
        py_lower: Bound<&Version>,
        py_upper: Bound<&Version>,
    ) -> NodeId {
        if matches!(i, NodeId::FALSE)
            || matches!((py_lower, py_upper), (Bound::Unbounded, Bound::Unbounded))
        {
            return i;
        }

        let py_range = Ranges::from_range_bounds((py_lower.cloned(), py_upper.cloned()));
        if py_range.is_empty() {
            // Oops, the bounds imply there is nothing that can match,
            // so we always evaluate to false.
            return NodeId::FALSE;
        }
        if matches!(i, NodeId::TRUE) {
            let var = Variable::Version(CanonicalMarkerValueVersion::PythonFullVersion);
            let edges = Edges::Version {
                edges: Edges::from_range(&py_range),
            };
            return self.create_node(var, edges).negate(i);
        }

        let node = self.shared.node(i);
        let Node {
            var: Variable::Version(CanonicalMarkerValueVersion::PythonFullVersion),
            children: Edges::Version { edges },
        } = node
        else {
            // Complexify all nodes recursively.
            let children = node.children.map(i, |node_id| {
                self.complexify_python_versions(node_id, py_lower, py_upper)
            });
            return self.create_node(node.var.clone(), children);
        };
        // The approach we take here is to filter out any range that
        // has no intersection with our Python lower/upper bounds.
        // These ranges will now always be false, so we can dismiss
        // them entirely.
        //
        // Then, depending on whether we have finite lower/upper bound,
        // we "fix up" the edges by clipping the existing ranges and
        // adding an additional range that covers the Python versions
        // outside of our bounds by always mapping them to false.
        let mut new: SmallVec<_> = edges
            .iter()
            .filter(|(range, _)| !py_range.intersection(range).is_empty())
            .cloned()
            .collect();
        // Below, we assume `new` has at least one element. It's
        // subtle, but since 1) edges is a disjoint covering of the
        // universe and 2) our `py_range` is non-empty at this point,
        // it must intersect with at least one range.
        assert!(
            !new.is_empty(),
            "expected at least one non-empty intersection"
        );
        // This is the NodeId we map to anything that should
        // always be false. We have to "negate" it in case the
        // parent is negated.
        let exclude_node_id = NodeId::FALSE.negate(i);
        if !matches!(py_lower, Bound::Unbounded) {
            let &(ref first_range, first_node_id) = new.first().unwrap();
            let first_upper = first_range.bounding_range().unwrap().1;
            // When the first range is always false, then we can just
            // "expand" it out to negative infinity to reflect that
            // anything less than our lower bound should evaluate to
            // false. If we don't do this, then we could have two
            // adjacent ranges map to the same node, which would not be
            // a canonical representation.
            if exclude_node_id == first_node_id {
                let clipped = Ranges::from_range_bounds((Bound::Unbounded, first_upper.cloned()));
                *new.first_mut().unwrap() = (clipped, first_node_id);
            } else {
                let clipped = Ranges::from_range_bounds((py_lower.cloned(), first_upper.cloned()));
                *new.first_mut().unwrap() = (clipped, first_node_id);

                let py_range_lower =
                    Ranges::from_range_bounds((py_lower.cloned(), Bound::Unbounded));
                new.insert(0, (py_range_lower.complement(), NodeId::FALSE.negate(i)));
            }
        }
        if !matches!(py_upper, Bound::Unbounded) {
            let &(ref last_range, last_node_id) = new.last().unwrap();
            let last_lower = last_range.bounding_range().unwrap().0;
            // See lower bound case above for why we do this. The
            // same reasoning applies here: to maintain a canonical
            // representation.
            if exclude_node_id == last_node_id {
                let clipped = Ranges::from_range_bounds((last_lower.cloned(), Bound::Unbounded));
                *new.last_mut().unwrap() = (clipped, last_node_id);
            } else {
                let clipped = Ranges::from_range_bounds((last_lower.cloned(), py_upper.cloned()));
                *new.last_mut().unwrap() = (clipped, last_node_id);

                let py_range_upper =
                    Ranges::from_range_bounds((Bound::Unbounded, py_upper.cloned()));
                new.push((py_range_upper.complement(), exclude_node_id));
            }
        }
        self.create_node(node.var.clone(), Edges::Version { edges: new })
            .negate(i)
    }

    /// The disjunction of known incompatible conditions.
    ///
    /// For example, while the marker specification and grammar do not _forbid_ it, we know that
    /// both `sys_platform == 'win32'` and `platform_system == 'Darwin'` will never true at the
    /// same time.
    ///
    /// This method thus encodes assumptions about the environment that are not guaranteed by the
    /// PEP 508 specification alone.
    fn exclusions(&mut self) -> NodeId {
        /// Perform a disjunction operation between two nodes.
        ///
        /// This is equivalent to [`InternerGuard::or`], with the exception that it does not
        /// incorporate knowledge from outside the marker algebra.
        fn disjunction(guard: &mut InternerGuard<'_>, xi: NodeId, yi: NodeId) -> NodeId {
            // We take advantage of cheap negation here and implement OR in terms
            // of it's De Morgan complement.
            conjunction(guard, xi.not(), yi.not()).not()
        }

        /// Perform a conjunction operation between two nodes.
        ///
        /// This is equivalent to [`InternerGuard::and`], with the exception that it does not
        /// incorporate knowledge from outside the marker algebra.
        fn conjunction(guard: &mut InternerGuard<'_>, xi: NodeId, yi: NodeId) -> NodeId {
            if xi.is_true() {
                return yi;
            }
            if yi.is_true() {
                return xi;
            }
            if xi == yi {
                return xi;
            }
            if xi.is_false() || yi.is_false() {
                return NodeId::FALSE;
            }
            // `X and not X` is `false` by definition.
            if xi.not() == yi {
                return NodeId::FALSE;
            }

            // The operation was memoized.
            if let Some(result) = guard.state.cache.get(&(xi, yi)) {
                return *result;
            }

            let (x, y) = (guard.shared.node(xi), guard.shared.node(yi));

            // Perform Shannon Expansion of the higher order variable.
            let (func, children) = match x.var.cmp(&y.var) {
                // X is higher order than Y, apply Y to every child of X.
                Ordering::Less => {
                    let children = x.children.map(xi, |node| conjunction(guard, node, yi));
                    (x.var.clone(), children)
                }
                // Y is higher order than X, apply X to every child of Y.
                Ordering::Greater => {
                    let children = y.children.map(yi, |node| conjunction(guard, node, xi));
                    (y.var.clone(), children)
                }
                // X and Y represent the same variable, merge their children.
                Ordering::Equal => {
                    let children = x
                        .children
                        .apply(xi, &y.children, yi, |x, y| conjunction(guard, x, y));
                    (x.var.clone(), children)
                }
            };

            // Create the output node.
            let node = guard.create_node(func, children);

            // Memoize the result of this operation.
            guard.state.cache.insert((xi, yi), node);

            node
        }

        if let Some(exclusions) = self.state.exclusions {
            return exclusions;
        }
        let mut tree = NodeId::FALSE;

        // Create all nodes upfront.
        let os_name_nt = self.expression(MarkerExpression::String {
            key: MarkerValueString::OsName,
            operator: MarkerOperator::Equal,
            value: arcstr::literal!("nt"),
        });
        let os_name_posix = self.expression(MarkerExpression::String {
            key: MarkerValueString::OsName,
            operator: MarkerOperator::Equal,
            value: arcstr::literal!("posix"),
        });
        let sys_platform_linux = self.expression(MarkerExpression::String {
            key: MarkerValueString::SysPlatform,
            operator: MarkerOperator::Equal,
            value: arcstr::literal!("linux"),
        });
        let sys_platform_darwin = self.expression(MarkerExpression::String {
            key: MarkerValueString::SysPlatform,
            operator: MarkerOperator::Equal,
            value: arcstr::literal!("darwin"),
        });
        let sys_platform_ios = self.expression(MarkerExpression::String {
            key: MarkerValueString::SysPlatform,
            operator: MarkerOperator::Equal,
            value: arcstr::literal!("ios"),
        });
        let sys_platform_win32 = self.expression(MarkerExpression::String {
            key: MarkerValueString::SysPlatform,
            operator: MarkerOperator::Equal,
            value: arcstr::literal!("win32"),
        });
        let platform_system_freebsd = self.expression(MarkerExpression::String {
            key: MarkerValueString::PlatformSystem,
            operator: MarkerOperator::Equal,
            value: arcstr::literal!("FreeBSD"),
        });
        let platform_system_netbsd = self.expression(MarkerExpression::String {
            key: MarkerValueString::PlatformSystem,
            operator: MarkerOperator::Equal,
            value: arcstr::literal!("NetBSD"),
        });
        let platform_system_openbsd = self.expression(MarkerExpression::String {
            key: MarkerValueString::PlatformSystem,
            operator: MarkerOperator::Equal,
            value: arcstr::literal!("OpenBSD"),
        });
        let platform_system_sunos = self.expression(MarkerExpression::String {
            key: MarkerValueString::PlatformSystem,
            operator: MarkerOperator::Equal,
            value: arcstr::literal!("SunOS"),
        });
        let platform_system_ios = self.expression(MarkerExpression::String {
            key: MarkerValueString::PlatformSystem,
            operator: MarkerOperator::Equal,
            value: arcstr::literal!("iOS"),
        });
        let platform_system_ipados = self.expression(MarkerExpression::String {
            key: MarkerValueString::PlatformSystem,
            operator: MarkerOperator::Equal,
            value: arcstr::literal!("iPadOS"),
        });
        let sys_platform_aix = self.expression(MarkerExpression::String {
            key: MarkerValueString::SysPlatform,
            operator: MarkerOperator::Equal,
            value: arcstr::literal!("aix"),
        });
        let sys_platform_android = self.expression(MarkerExpression::String {
            key: MarkerValueString::SysPlatform,
            operator: MarkerOperator::Equal,
            value: arcstr::literal!("android"),
        });
        let sys_platform_emscripten = self.expression(MarkerExpression::String {
            key: MarkerValueString::SysPlatform,
            operator: MarkerOperator::Equal,
            value: arcstr::literal!("emscripten"),
        });
        let sys_platform_cygwin = self.expression(MarkerExpression::String {
            key: MarkerValueString::SysPlatform,
            operator: MarkerOperator::Equal,
            value: arcstr::literal!("cygwin"),
        });
        let sys_platform_wasi = self.expression(MarkerExpression::String {
            key: MarkerValueString::SysPlatform,
            operator: MarkerOperator::Equal,
            value: arcstr::literal!("wasi"),
        });

        // Pairs of `os_name` and `sys_platform` that are known to be incompatible.
        //
        // For example: `os_name == 'nt' and sys_platform == 'darwin'`
        let mut pairs = vec![
            (os_name_nt, sys_platform_linux),
            (os_name_nt, sys_platform_darwin),
            (os_name_nt, sys_platform_ios),
            (os_name_posix, sys_platform_win32),
        ];

        // Pairs of `platform_system` and `sys_platform` that are known to be incompatible.
        //
        // For example: `platform_system == 'FreeBSD' and sys_platform == 'aix'`
        for platform_system in [
            platform_system_freebsd,
            platform_system_netbsd,
            platform_system_openbsd,
            platform_system_sunos,
            platform_system_ios,
            platform_system_ipados,
        ] {
            for sys_platform in [
                sys_platform_aix,
                sys_platform_android,
                sys_platform_emscripten,
                sys_platform_ios,
                sys_platform_linux,
                sys_platform_darwin,
                sys_platform_win32,
                sys_platform_cygwin,
                sys_platform_wasi,
            ] {
                // Some of the above pairs are actually compatible.
                if sys_platform == sys_platform_ios
                    && (platform_system == platform_system_ios
                        || platform_system == platform_system_ipados)
                {
                    continue;
                }
                pairs.push((platform_system, sys_platform));
            }
        }

        for (a, b) in pairs {
            let a_and_b = conjunction(self, a, b);
            tree = disjunction(self, tree, a_and_b);
        }

        self.state.exclusions = Some(tree);
        tree
    }
}

/// A unique variable for a decision node.
///
/// This `enum` also defines the variable ordering for all ADDs.
/// Variable ordering is an interesting property of ADDs. A bad ordering
/// can lead to exponential explosion of the size of an ADD. However,
/// dynamically computing an optimal ordering is NP-complete.
///
/// We may wish to investigate the effect of this ordering on common marker
/// trees. However, marker trees are typically small, so this may not be high
/// impact.
#[derive(PartialOrd, Ord, PartialEq, Eq, Hash, Clone, Debug)]
pub(crate) enum Variable {
    /// A string marker, such as `os_name`.
    String(CanonicalMarkerValueString),
    /// A version marker, such as `python_version`.
    ///
    /// This is the highest order variable as it typically contains the most complex
    /// ranges, allowing us to merge ranges at the top-level.
    Version(CanonicalMarkerValueVersion),
    /// A variable representing a `<key> in <value>` expression for a particular
    /// string marker and value.
    In {
        key: CanonicalMarkerValueString,
        value: ArcStr,
    },
    /// A variable representing a `<value> in <key>` expression for a particular
    /// string marker and value.
    Contains {
        key: CanonicalMarkerValueString,
        value: ArcStr,
    },
    /// A variable representing the existence or absence of a given extra.
    ///
    /// We keep extras at the leaves of the tree, so when simplifying extras we can
    /// trivially remove the leaves without having to reconstruct the entire tree.
    Extra(CanonicalMarkerValueExtra),
    /// A variable representing whether a `<value> in <key>` or `<value> not in <key>`
    /// expression, where the key is a list.
    ///
    /// We keep extras and groups at the leaves of the tree, so when simplifying extras we can
    /// trivially remove the leaves without having to reconstruct the entire tree.
    List(CanonicalMarkerListPair),
}

impl Variable {
    /// Returns `true` if the variable is known to be involved in _at least_ one conflicting
    /// marker pair.
    ///
    /// For example, `sys_platform == 'win32'` and `platform_system == 'Darwin'` are known to
    /// never be true at the same time.
    fn is_conflicting_variable(&self) -> bool {
        let Variable::String(marker) = self else {
            return false;
        };
        marker.is_conflicting()
    }
}

/// A decision node in an Algebraic Decision Diagram.
#[derive(PartialEq, Eq, Hash, Clone, Debug)]
pub(crate) struct Node {
    /// The variable this node represents.
    pub(crate) var: Variable,
    /// The children of this node, with edges representing the possible outputs
    /// of this variable.
    pub(crate) children: Edges,
}

impl Node {
    /// Return the complement of this node, flipping all children IDs.
    fn not(self) -> Node {
        Node {
            var: self.var,
            children: self.children.not(),
        }
    }
}

/// An ID representing a reference to a decision node in the [`Interner`].
///
/// The lowest bit of the ID is used represent complemented edges.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct NodeId(usize);

impl NodeId {
    // The terminal node representing `true`, or a trivially `true` node.
    pub(crate) const TRUE: NodeId = NodeId(0);

    // The terminal node representing `false`, or an unsatisifable node.
    pub(crate) const FALSE: NodeId = NodeId(1);

    /// Create a new, optionally complemented, [`NodeId`] with the given index.
    fn new(index: usize, complement: bool) -> NodeId {
        // Ensure the index does not interfere with the lowest complement bit.
        let index = (index + 1) << 1;
        NodeId(index | usize::from(complement))
    }

    /// Returns the index of this ID, ignoring the complemented edge.
    fn index(self) -> usize {
        // Ignore the lowest bit and bring indices back to starting at `0`.
        (self.0 >> 1) - 1
    }

    /// Returns `true` if this ID represents a complemented edge.
    fn is_complement(self) -> bool {
        // Whether the lowest bit is set.
        (self.0 & 1) == 1
    }

    /// Returns the complement of this node.
    pub(crate) fn not(self) -> NodeId {
        // Toggle the lowest bit.
        NodeId(self.0 ^ 1)
    }

    /// Returns the complement of this node, if it's parent is complemented.
    ///
    /// This method is useful to restore the complemented state of children nodes
    /// when traversing the tree.
    pub(crate) fn negate(self, parent: NodeId) -> NodeId {
        if parent.is_complement() {
            self.not()
        } else {
            self
        }
    }

    /// Returns `true` if this node represents an unsatisfiable node.
    pub(crate) fn is_false(self) -> bool {
        self == NodeId::FALSE
    }

    /// Returns `true` if this node represents a trivially `true` node.
    pub(crate) fn is_true(self) -> bool {
        self == NodeId::TRUE
    }
}

/// A [`SmallVec`] with enough elements to hold two constant edges, as well as the
/// ranges in-between.
type SmallVec<T> = smallvec::SmallVec<[T; 5]>;

/// The edges of a decision node.
#[derive(PartialEq, Eq, Hash, Clone, Debug)]
#[allow(clippy::large_enum_variant)] // Nodes are interned.
pub(crate) enum Edges {
    // The edges of a version variable, representing a disjoint set of ranges that cover
    // the output space.
    //
    // Invariant: All ranges are simple, meaning they can be represented by a bounded
    // interval without gaps. Additionally, there are at least two edges in the set.
    Version {
        edges: SmallVec<(Ranges<Version>, NodeId)>,
    },
    // The edges of a string variable, representing a disjoint set of ranges that cover
    // the output space.
    //
    // Invariant: All ranges are simple, meaning they can be represented by a bounded
    // interval without gaps. Additionally, there are at least two edges in the set.
    String {
        edges: SmallVec<(Ranges<ArcStr>, NodeId)>,
    },
    // The edges of a boolean variable, representing the values `true` (the `high` child)
    // and `false` (the `low` child).
    Boolean {
        high: NodeId,
        low: NodeId,
    },
}

impl Edges {
    /// Returns the [`Edges`] for a boolean variable.
    fn from_bool(complemented: bool) -> Edges {
        if complemented {
            Edges::Boolean {
                high: NodeId::TRUE,
                low: NodeId::FALSE,
            }
        } else {
            Edges::Boolean {
                high: NodeId::FALSE,
                low: NodeId::TRUE,
            }
        }
    }

    /// Returns the [`Edges`] for a string expression.
    ///
    /// This function will panic for the `In` and `Contains` marker operators, which
    /// should be represented as separate boolean variables.
    fn from_string(operator: MarkerOperator, value: ArcStr) -> Edges {
        let range: Ranges<ArcStr> = match operator {
            MarkerOperator::Equal => Ranges::singleton(value),
            MarkerOperator::NotEqual => Ranges::singleton(value).complement(),
            MarkerOperator::GreaterThan => Ranges::strictly_higher_than(value),
            MarkerOperator::GreaterEqual => Ranges::higher_than(value),
            MarkerOperator::LessThan => Ranges::strictly_lower_than(value),
            MarkerOperator::LessEqual => Ranges::lower_than(value),
            MarkerOperator::TildeEqual => unreachable!("string comparisons with ~= are ignored"),
            _ => unreachable!("`in` and `contains` are treated as boolean variables"),
        };

        Edges::String {
            edges: Edges::from_range(&range),
        }
    }

    /// Returns the [`Edges`] for a version specifier.
    fn from_specifier(specifier: VersionSpecifier) -> Edges {
        let specifier = release_specifier_to_range(specifier.only_release(), true);
        Edges::Version {
            edges: Edges::from_range(&specifier),
        }
    }

    /// Returns an [`Edges`] where values in the given range are `true`.
    ///
    /// Only for use when the `key` is a `PythonVersion`. Normalizes to `PythonFullVersion`.
    fn from_python_versions(
        versions: Vec<Version>,
        operator: ContainerOperator,
    ) -> Result<Edges, NodeId> {
        let mut range: Ranges<Version> = versions
            .into_iter()
            .map(|version| {
                let specifier = VersionSpecifier::equals_version(version.only_release());
                let specifier = python_version_to_full_version(specifier)?;
                Ok(release_specifier_to_range(specifier, true))
            })
            .flatten_ok()
            .collect::<Result<Ranges<_>, NodeId>>()?;

        if operator == ContainerOperator::NotIn {
            range = range.complement();
        }

        Ok(Edges::Version {
            edges: Edges::from_range(&range),
        })
    }

    /// Returns an [`Edges`] where values in the given range are `true`.
    fn from_versions(versions: &[Version], operator: ContainerOperator) -> Edges {
        let mut range: Ranges<Version> = versions
            .iter()
            .map(|version| {
                (
                    Bound::Included(version.clone()),
                    Bound::Included(version.clone()),
                )
            })
            .collect();

        if operator == ContainerOperator::NotIn {
            range = range.complement();
        }

        Edges::Version {
            edges: Edges::from_range(&range),
        }
    }

    /// Returns an [`Edges`] where values in the given range are `true`.
    fn from_range<T>(range: &Ranges<T>) -> SmallVec<(Ranges<T>, NodeId)>
    where
        T: Ord + Clone,
    {
        let mut edges = SmallVec::new();

        // Add the `true` edges.
        for (start, end) in range.iter() {
            let range = Ranges::from_range_bounds((start.clone(), end.clone()));
            edges.push((range, NodeId::TRUE));
        }

        // Add the `false` edges.
        for (start, end) in range.complement().iter() {
            let range = Ranges::from_range_bounds((start.clone(), end.clone()));
            edges.push((range, NodeId::FALSE));
        }

        // Sort the ranges.
        //
        // The ranges are disjoint so we don't care about equality.
        edges.sort_by(|(range1, _), (range2, _)| compare_disjoint_range_start(range1, range2));
        edges
    }

    /// Merge two [`Edges`], applying the given operation (e.g., `AND` or `OR`) to all intersecting edges.
    ///
    /// For example, given two nodes corresponding to the same boolean variable:
    /// ```text
    /// left  (extra == 'foo'): { true: A, false: B }
    /// right (extra == 'foo'): { true: C, false: D }
    /// ```
    ///
    /// We merge them into a single node by applying the given operation to the matching edges.
    /// ```text
    /// (extra == 'foo'): { true: (A and C), false: (B and D) }
    /// ```
    /// For non-boolean variables, this is more complex. See `apply_ranges` for details.
    ///
    /// Note that the LHS and RHS must be of the same [`Edges`] variant.
    fn apply(
        &self,
        parent: NodeId,
        right_edges: &Edges,
        right_parent: NodeId,
        mut apply: impl FnMut(NodeId, NodeId) -> NodeId,
    ) -> Edges {
        match (self, right_edges) {
            // For version or string variables, we have to split and merge the overlapping ranges.
            (Edges::Version { edges }, Edges::Version { edges: right_edges }) => Edges::Version {
                edges: Edges::apply_ranges(edges, parent, right_edges, right_parent, apply),
            },
            (Edges::String { edges }, Edges::String { edges: right_edges }) => Edges::String {
                edges: Edges::apply_ranges(edges, parent, right_edges, right_parent, apply),
            },
            // For boolean variables, we simply merge the low and high edges.
            (
                Edges::Boolean { high, low },
                Edges::Boolean {
                    high: right_high,
                    low: right_low,
                },
            ) => Edges::Boolean {
                high: apply(high.negate(parent), right_high.negate(right_parent)),
                low: apply(low.negate(parent), right_low.negate(right_parent)),
            },
            _ => unreachable!("cannot merge two `Edges` of different types"),
        }
    }

    /// Merge two range maps, applying the given operation to all disjoint, intersecting ranges.
    ///
    /// For example, two nodes might have the following edges:
    /// ```text
    /// left  (python_version): { [0, 3.4): A,   [3.4, 3.4]: B,   (3.4, inf): C }
    /// right (python_version): { [0, 3.6): D,   [3.6, 3.6]: E,   (3.6, inf): F }
    /// ```
    ///
    /// Unlike with boolean variables, we can't simply apply the operation the static `true`
    /// and `false` edges. Instead, we have to split and merge overlapping ranges:
    /// ```text
    /// python_version: {
    ///     [0, 3.4):   (A and D),
    ///     [3.4, 3.4]: (B and D),
    ///     (3.4, 3.6): (C and D),
    ///     [3.6, 3.6]: (C and E),
    ///     (3.6, inf): (C and F)
    /// }
    /// ```
    ///
    /// The left and right edges may also have a restricted range from calls to `restrict_versions`.
    /// In that case, we drop any ranges that do not exist in the domain of both edges. Note that
    /// this should not occur in practice because `requires-python` bounds are global.
    fn apply_ranges<T>(
        left_edges: &SmallVec<(Ranges<T>, NodeId)>,
        left_parent: NodeId,
        right_edges: &SmallVec<(Ranges<T>, NodeId)>,
        right_parent: NodeId,
        mut apply: impl FnMut(NodeId, NodeId) -> NodeId,
    ) -> SmallVec<(Ranges<T>, NodeId)>
    where
        T: Clone + Ord,
    {
        let mut combined = SmallVec::new();
        for (left_range, left_child) in left_edges {
            // Split the two maps into a set of disjoint and overlapping ranges, merging the
            // intersections.
            //
            // Note that restrict ranges (see `restrict_versions`) makes finding intersections
            // a bit more complicated despite the ranges being sorted. We cannot simply zip both
            // sets, as they may contain arbitrary gaps. Instead, we use a quadratic search for
            // simplicity as the set of ranges for a given variable is typically very small.
            for (right_range, right_child) in right_edges {
                let intersection = right_range.intersection(left_range);
                if intersection.is_empty() {
                    // TODO(ibraheem): take advantage of the sorted ranges to `break` early
                    continue;
                }

                // Merge the intersection.
                let node = apply(
                    left_child.negate(left_parent),
                    right_child.negate(right_parent),
                );

                match combined.last_mut() {
                    // Combine ranges if possible.
                    Some((range, prev)) if *prev == node && can_conjoin(range, &intersection) => {
                        *range = range.union(&intersection);
                    }
                    _ => combined.push((intersection.clone(), node)),
                }
            }
        }

        combined
    }

    // Returns `true` if two [`Edges`] are disjoint.
    fn is_disjoint(
        &self,
        parent: NodeId,
        right_edges: &Edges,
        right_parent: NodeId,
        interner: &mut InternerGuard<'_>,
    ) -> bool {
        match (self, right_edges) {
            // For version or string variables, we have to split and check the overlapping ranges.
            (Edges::Version { edges }, Edges::Version { edges: right_edges }) => {
                Edges::is_disjoint_ranges(edges, parent, right_edges, right_parent, interner)
            }
            (Edges::String { edges }, Edges::String { edges: right_edges }) => {
                Edges::is_disjoint_ranges(edges, parent, right_edges, right_parent, interner)
            }
            // For boolean variables, we simply check the low and high edges.
            (
                Edges::Boolean { high, low },
                Edges::Boolean {
                    high: right_high,
                    low: right_low,
                },
            ) => {
                interner.disjointness(high.negate(parent), right_high.negate(right_parent))
                    && interner.disjointness(low.negate(parent), right_low.negate(right_parent))
            }
            _ => unreachable!("cannot merge two `Edges` of different types"),
        }
    }

    // Returns `true` if all intersecting ranges in two range maps are disjoint.
    fn is_disjoint_ranges<T>(
        left_edges: &SmallVec<(Ranges<T>, NodeId)>,
        left_parent: NodeId,
        right_edges: &SmallVec<(Ranges<T>, NodeId)>,
        right_parent: NodeId,
        interner: &mut InternerGuard<'_>,
    ) -> bool
    where
        T: Clone + Ord,
    {
        // This is similar to the routine in `apply_ranges` except we only care about disjointness,
        // not the resulting edges.
        for (left_range, left_child) in left_edges {
            for (right_range, right_child) in right_edges {
                if right_range.is_disjoint(left_range) {
                    continue;
                }

                // Ensure the intersection is disjoint.
                if !interner.disjointness(
                    left_child.negate(left_parent),
                    right_child.negate(right_parent),
                ) {
                    return false;
                }
            }
        }

        true
    }

    // Apply the given function to all direct children of this node.
    fn map(&self, parent: NodeId, mut f: impl FnMut(NodeId) -> NodeId) -> Edges {
        match self {
            Edges::Version { edges: map } => Edges::Version {
                edges: map
                    .iter()
                    .cloned()
                    .map(|(range, node)| (range, f(node.negate(parent))))
                    .collect(),
            },
            Edges::String { edges: map } => Edges::String {
                edges: map
                    .iter()
                    .cloned()
                    .map(|(range, node)| (range, f(node.negate(parent))))
                    .collect(),
            },
            Edges::Boolean { high, low } => Edges::Boolean {
                low: f(low.negate(parent)),
                high: f(high.negate(parent)),
            },
        }
    }

    // Returns an iterator over all direct children of this node.
    fn nodes(&self) -> impl Iterator<Item = NodeId> + '_ {
        match self {
            Edges::Version { edges: map } => {
                Either::Left(Either::Left(map.iter().map(|(_, node)| *node)))
            }
            Edges::String { edges: map } => {
                Either::Left(Either::Right(map.iter().map(|(_, node)| *node)))
            }
            Edges::Boolean { high, low } => Either::Right([*high, *low].into_iter()),
        }
    }

    // Returns the complement of this [`Edges`].
    fn not(self) -> Edges {
        match self {
            Edges::Version { edges: map } => Edges::Version {
                edges: map
                    .into_iter()
                    .map(|(range, node)| (range, node.not()))
                    .collect(),
            },
            Edges::String { edges: map } => Edges::String {
                edges: map
                    .into_iter()
                    .map(|(range, node)| (range, node.not()))
                    .collect(),
            },
            Edges::Boolean { high, low } => Edges::Boolean {
                high: high.not(),
                low: low.not(),
            },
        }
    }
}

/// Returns the equivalent `python_full_version` specifier for a `python_version` specifier.
///
/// Returns `Err` with a constant node if the equivalent comparison is always `true` or `false`.
fn python_version_to_full_version(specifier: VersionSpecifier) -> Result<VersionSpecifier, NodeId> {
    // Trailing zeroes matter only for (not-)equals-star and tilde-equals. This means that below
    // the next two blocks, we can use the trimmed release as the release.
    if specifier.operator().is_star() {
        // Input          python_version  python_full_version
        // ==3.*          3.*             3.*
        // ==3.0.*        3.0             3.0.*
        // ==3.0.0.*      3.0             3.0.*
        // ==3.9.*        3.9             3.9.*
        // ==3.9.0.*      3.9             3.9.*
        // ==3.9.0.0.*    3.9             3.9.*
        // ==3.9.1.*      FALSE           FALSE
        // ==3.9.1.0.*    FALSE           FALSE
        // ==3.9.1.0.0.*  FALSE           FALSE
        return match &*specifier.version().release() {
            // `3.*`
            [_major] => Ok(specifier),
            // Ex) `3.9.*`, `3.9.0.*`, or `3.9.0.0.*`
            [major, minor, rest @ ..] if rest.iter().all(|x| *x == 0) => {
                let python_version = Version::new([major, minor]);
                // Unwrap safety: A star operator with two version segments is always valid.
                Ok(VersionSpecifier::from_version(*specifier.operator(), python_version).unwrap())
            }
            // Ex) `3.9.1.*` or `3.9.0.1.*`
            _ => Err(NodeId::FALSE),
        };
    }

    if *specifier.operator() == Operator::TildeEqual {
        // python_version  python_full_version
        // ~=3             (not possible)
        // ~= 3.0          >= 3.0, < 4.0
        // ~= 3.9          >= 3.9, < 4.0
        // ~= 3.9.0        == 3.9.*
        // ~= 3.9.1        FALSE
        // ~= 3.9.0.0      == 3.9.*
        // ~= 3.9.0.1      FALSE
        return match &*specifier.version().release() {
            // Ex) `3.0`, `3.7`
            [_major, _minor] => Ok(specifier),
            // Ex) `3.9`, `3.9.0`, or `3.9.0.0`
            [major, minor, rest @ ..] if rest.iter().all(|x| *x == 0) => {
                let python_version = Version::new([major, minor]);
                Ok(VersionSpecifier::equals_star_version(python_version))
            }
            // Ex) `3.9.1` or `3.9.0.1`
            _ => Err(NodeId::FALSE),
        };
    }

    // Extract the major and minor version segments if the specifier contains exactly
    // those segments, or if it contains a major segment with an implied minor segment of `0`.
    let major_minor = match *specifier.version().only_release_trimmed().release() {
        // Add a trailing `0` for the minor version, which is implied.
        // For example, `python_version == 3` matches `3.0.1`, `3.0.2`, etc.
        [major] => Some((major, 0)),
        [major, minor] => Some((major, minor)),
        // Specifiers including segments beyond the minor version require separate handling.
        _ => None,
    };

    // Note that the values taken on by `python_version` are truncated to their major and minor
    // version segments. For example, a python version of `3.7.0`, `3.7.1`, and so on, would all
    // result in a `python_version` marker of `3.7`. For this reason, we must consider the range
    // of values that would satisfy a `python_version` specifier when truncated in order to transform
    // the specifier into its `python_full_version` equivalent.
    if let Some((major, minor)) = major_minor {
        let version = Version::new([major, minor]);

        Ok(match specifier.operator() {
            // `python_version == 3.7` is equivalent to `python_full_version == 3.7.*`.
            Operator::Equal | Operator::ExactEqual => {
                VersionSpecifier::equals_star_version(version)
            }
            // `python_version != 3.7` is equivalent to `python_full_version != 3.7.*`.
            Operator::NotEqual => VersionSpecifier::not_equals_star_version(version),

            // `python_version > 3.7` is equivalent to `python_full_version >= 3.8`.
            Operator::GreaterThan => {
                VersionSpecifier::greater_than_equal_version(Version::new([major, minor + 1]))
            }
            // `python_version < 3.7` is equivalent to `python_full_version < 3.7`.
            Operator::LessThan => specifier,
            // `python_version >= 3.7` is equivalent to `python_full_version >= 3.7`.
            Operator::GreaterThanEqual => specifier,
            // `python_version <= 3.7` is equivalent to `python_full_version < 3.8`.
            Operator::LessThanEqual => {
                VersionSpecifier::less_than_version(Version::new([major, minor + 1]))
            }

            Operator::EqualStar | Operator::NotEqualStar | Operator::TildeEqual => {
                // Handled above.
                unreachable!()
            }
        })
    } else {
        let [major, minor, ..] = *specifier.version().release() else {
            unreachable!()
        };

        Ok(match specifier.operator() {
            // `python_version` cannot have more than two release segments, and we know
            // that the following release segments aren't purely zeroes so equality is impossible.
            Operator::Equal | Operator::ExactEqual => {
                return Err(NodeId::FALSE);
            }

            // Similarly, inequalities are always `true`.
            Operator::NotEqual => return Err(NodeId::TRUE),

            // `python_version {<,<=} 3.7.8` is equivalent to `python_full_version < 3.8`.
            Operator::LessThan | Operator::LessThanEqual => {
                VersionSpecifier::less_than_version(Version::new([major, minor + 1]))
            }

            // `python_version {>,>=} 3.7.8` is equivalent to `python_full_version >= 3.8`.
            Operator::GreaterThan | Operator::GreaterThanEqual => {
                VersionSpecifier::greater_than_equal_version(Version::new([major, minor + 1]))
            }

            Operator::EqualStar | Operator::NotEqualStar | Operator::TildeEqual => {
                // Handled above.
                unreachable!()
            }
        })
    }
}

/// Compares the start of two ranges that are known to be disjoint.
fn compare_disjoint_range_start<T>(range1: &Ranges<T>, range2: &Ranges<T>) -> Ordering
where
    T: Ord,
{
    let (upper1, _) = range1.bounding_range().unwrap();
    let (upper2, _) = range2.bounding_range().unwrap();

    match (upper1, upper2) {
        (Bound::Unbounded, _) => Ordering::Less,
        (_, Bound::Unbounded) => Ordering::Greater,
        (Bound::Included(v1), Bound::Excluded(v2)) if v1 == v2 => Ordering::Less,
        (Bound::Excluded(v1), Bound::Included(v2)) if v1 == v2 => Ordering::Greater,
        // Note that the ranges are disjoint, so their lower bounds cannot be equal.
        (Bound::Included(v1) | Bound::Excluded(v1), Bound::Included(v2) | Bound::Excluded(v2)) => {
            v1.cmp(v2)
        }
    }
}

/// Returns `true` if two disjoint ranges can be conjoined seamlessly without introducing a gap.
fn can_conjoin<T>(range1: &Ranges<T>, range2: &Ranges<T>) -> bool
where
    T: Ord + Clone,
{
    let Some((_, end)) = range1.bounding_range() else {
        return false;
    };
    let Some((start, _)) = range2.bounding_range() else {
        return false;
    };

    match (end, start) {
        (Bound::Included(v1), Bound::Excluded(v2)) if v1 == v2 => true,
        (Bound::Excluded(v1), Bound::Included(v2)) if v1 == v2 => true,
        _ => false,
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_false() {
            return write!(f, "false");
        }

        if self.is_true() {
            return write!(f, "true");
        }

        if self.is_complement() {
            write!(f, "{:?}", INTERNER.shared.node(*self).clone().not())
        } else {
            write!(f, "{:?}", INTERNER.shared.node(*self))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{INTERNER, NodeId};
    use crate::MarkerExpression;

    fn expr(s: &str) -> NodeId {
        INTERNER
            .lock()
            .expression(MarkerExpression::from_str(s).unwrap().unwrap())
    }

    #[test]
    fn basic() {
        let m = || INTERNER.lock();
        let extra_foo = expr("extra == 'foo'");
        assert!(!extra_foo.is_false());

        let os_foo = expr("os_name == 'foo'");
        let extra_and_os_foo = m().or(extra_foo, os_foo);
        assert!(!extra_and_os_foo.is_false());
        assert!(!m().and(extra_foo, os_foo).is_false());

        let trivially_true = m().or(extra_and_os_foo, extra_and_os_foo.not());
        assert!(!trivially_true.is_false());
        assert!(trivially_true.is_true());

        let trivially_false = m().and(extra_foo, extra_foo.not());
        assert!(trivially_false.is_false());

        let e = m().or(trivially_false, os_foo);
        assert!(!e.is_false());

        let extra_not_foo = expr("extra != 'foo'");
        assert!(m().and(extra_foo, extra_not_foo).is_false());
        assert!(m().or(extra_foo, extra_not_foo).is_true());

        let os_geq_bar = expr("os_name >= 'bar'");
        assert!(!os_geq_bar.is_false());

        let os_le_bar = expr("os_name < 'bar'");
        assert!(m().and(os_geq_bar, os_le_bar).is_false());
        assert!(m().or(os_geq_bar, os_le_bar).is_true());

        let os_leq_bar = expr("os_name <= 'bar'");
        assert!(!m().and(os_geq_bar, os_leq_bar).is_false());
        assert!(m().or(os_geq_bar, os_leq_bar).is_true());
    }

    #[test]
    fn version() {
        let m = || INTERNER.lock();
        let eq_3 = expr("python_version == '3'");
        let neq_3 = expr("python_version != '3'");
        let geq_3 = expr("python_version >= '3'");
        let leq_3 = expr("python_version <= '3'");

        let eq_2 = expr("python_version == '2'");
        let eq_1 = expr("python_version == '1'");
        assert!(m().and(eq_2, eq_1).is_false());

        assert_eq!(eq_3.not(), neq_3);
        assert_eq!(eq_3, neq_3.not());

        assert!(m().and(eq_3, neq_3).is_false());
        assert!(m().or(eq_3, neq_3).is_true());

        assert_eq!(m().and(eq_3, geq_3), eq_3);
        assert_eq!(m().and(eq_3, leq_3), eq_3);

        assert_eq!(m().and(geq_3, leq_3), eq_3);

        assert!(!m().and(geq_3, leq_3).is_false());
        assert!(m().or(geq_3, leq_3).is_true());
    }

    #[test]
    fn simplify() {
        let m = || INTERNER.lock();
        let x86 = expr("platform_machine == 'x86_64'");
        let not_x86 = expr("platform_machine != 'x86_64'");
        let windows = expr("platform_machine == 'Windows'");

        let a = m().and(x86, windows);
        let b = m().and(not_x86, windows);
        assert_eq!(m().or(a, b), windows);
    }
}
