use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use tina::{Address, AddressGeneration, IsolateId, ShardId, SystemIncarnation};

#[test]
fn address_identity_traits_include_system_incarnation() {
    let address = |system| {
        Address::<u8>::new_with_generation_in(
            SystemIncarnation::new(system),
            ShardId::new(2),
            IsolateId::new(3),
            AddressGeneration::new(4),
        )
    };
    let first = address(10);
    let second = address(11);

    assert_ne!(first, second);
    assert!(first < second);

    let hash = |value: Address<u8>| {
        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        hasher.finish()
    };
    assert_ne!(hash(first), hash(second));
    assert!(format!("{first:?}").contains("SystemIncarnation(10)"));
}

#[test]
fn address_is_copy_and_has_the_expected_inline_layout() {
    fn assert_copy<T: Copy>() {}
    assert_copy::<Address<u8, u16>>();

    #[cfg(target_pointer_width = "64")]
    assert_eq!(std::mem::size_of::<Address<u8, u16>>(), 32);
}
