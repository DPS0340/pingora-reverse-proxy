use pingora_load_balancing::Backend;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

#[derive(Default)]
struct RecordingHasher(Vec<u8>);

impl Hasher for RecordingHasher {
    fn finish(&self) -> u64 {
        0
    }

    fn write(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }
}

fn hash(backend: &Backend) -> u64 {
    let mut hasher = DefaultHasher::new();
    backend.hash(&mut hasher);
    hasher.finish()
}

fn hash_input(backend: &Backend) -> Vec<u8> {
    let mut hasher = RecordingHasher::default();
    backend.hash(&mut hasher);
    hasher.0
}

#[test]
fn patched_backend_identity_ignores_extensions() {
    let mut left = Backend::new_with_weight("1.1.1.1:80", 2).expect("left backend");
    let mut right = Backend::new_with_weight("1.1.1.1:80", 2).expect("right backend");
    left.ext.insert(true);
    right.ext.insert(7_u8);

    assert_eq!(left, right);
    assert_eq!(left.cmp(&right), std::cmp::Ordering::Equal);
    assert_eq!(left.partial_cmp(&right), Some(std::cmp::Ordering::Equal));
    assert_eq!(hash(&left), hash(&right));
    assert_eq!(hash_input(&left), hash_input(&right));

    let clone = left.clone();
    assert_eq!(clone.ext.get::<bool>(), Some(&true));
    assert!(format!("{left:?}").contains("ext:"));
}

#[test]
fn patched_backend_identity_retains_address_and_weight() {
    let base = Backend::new_with_weight("1.1.1.1:80", 2).expect("base backend");
    let different_address =
        Backend::new_with_weight("1.1.1.2:80", 2).expect("different-address backend");
    let different_weight =
        Backend::new_with_weight("1.1.1.1:80", 3).expect("different-weight backend");

    for different in [&different_address, &different_weight] {
        assert_ne!(&base, different);
        assert_ne!(base.cmp(different), std::cmp::Ordering::Equal);
        assert_eq!(base.partial_cmp(different), Some(base.cmp(different)));
        assert_ne!(hash_input(&base), hash_input(different));
    }
}
