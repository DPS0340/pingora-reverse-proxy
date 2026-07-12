//! Immutable, segment-indexed route matching.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::route::{RouteData, RouteKey};

#[derive(Clone, Debug, Default)]
struct Node {
    children: BTreeMap<String, Node>,
    route: Option<RouteMatch>,
}

/// A matched normalized key and shared immutable route data.
#[derive(Clone, Debug, PartialEq)]
pub struct RouteMatch {
    pub key: RouteKey,
    pub data: Arc<RouteData>,
}

/// An immutable snapshot optimized for segment-prefix lookups.
#[derive(Clone, Debug, Default)]
pub struct RouteSnapshot {
    root: Node,
}

impl RouteSnapshot {
    /// Build a new immutable matcher from a complete logical route map.
    pub fn from_routes(routes: BTreeMap<RouteKey, RouteData>) -> Self {
        let mut root = Node::default();

        for (key, data) in routes {
            let mut node = &mut root;
            for segment in segments(key.as_str()) {
                node = node.children.entry(segment.to_owned()).or_default();
            }
            node.route = Some(RouteMatch {
                key,
                data: Arc::new(data),
            });
        }

        Self { root }
    }

    /// Resolve a request path to the deepest stored route on segment boundaries.
    pub fn resolve(&self, request_path: &str) -> Option<RouteMatch> {
        let mut node = &self.root;
        let mut matched = node.route.as_ref();

        for segment in segments(request_path) {
            let Some(child) = node.children.get(segment) else {
                break;
            };
            node = child;
            if node.route.is_some() {
                matched = node.route.as_ref();
            }
        }

        matched.cloned()
    }
}

fn segments(path: &str) -> impl Iterator<Item = &str> {
    let path = path.trim_matches('/');
    path.split('/').filter(move |_| !path.is_empty())
}
