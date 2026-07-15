use pingora_load_balancing::Backend;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

fn hash(backend: &Backend) -> u64 {
    let mut hasher = DefaultHasher::new();
    backend.hash(&mut hasher);
    hasher.finish()
}

#[test]
fn patched_backend_identity_ignores_extensions() {
    let mut left = Backend::new_with_weight("1.1.1.1:80", 2).expect("left backend");
    let mut right = Backend::new_with_weight("1.1.1.1:80", 2).expect("right backend");
    left.ext.insert(true);
    right.ext.insert(7_u8);

    assert_eq!(left, right);
    assert_eq!(left.cmp(&right), std::cmp::Ordering::Equal);
    assert_eq!(hash(&left), hash(&right));

    let clone = left.clone();
    assert_eq!(clone.ext.get::<bool>(), Some(&true));
}
