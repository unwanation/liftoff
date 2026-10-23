mod check_kv_key {
    use super::super::{KvRefusal, MAX_KV_KEY_BYTES, check_kv_key};

    #[test]
    fn accepts_key_when_it_is_at_limit() {
        // TC: none a key past 256 bytes)
        assert_eq!(check_kv_key(&"k".repeat(MAX_KV_KEY_BYTES)), Ok(()));
    }

    #[test]
    fn refuses_key_when_it_is_past_limit() {
        // TC: none a key past 256 bytes)
        assert_eq!(
            check_kv_key(&"k".repeat(MAX_KV_KEY_BYTES + 1)),
            Err(KvRefusal::KeyTooLong)
        );
    }
}

mod check_kv_write {
    use super::super::{KvRefusal, KvWrite, MAX_KV_KEY_BYTES, check_kv_write};

    const fn write(key: &str, old: Option<usize>, new: usize) -> KvWrite<'_> {
        KvWrite { key, old, new }
    }

    #[test]
    fn counts_key_and_value_when_key_is_new() {
        // TC: FN-04
        assert_eq!(check_kv_write(10, [write("abc", None, 5)], 100), Ok(18));
    }

    #[test]
    fn counts_only_difference_when_key_is_replaced() {
        // TC: FN-04
        assert_eq!(check_kv_write(18, [write("abc", Some(5), 2)], 100), Ok(15));
    }

    #[test]
    fn accepts_write_when_it_reaches_limit_exactly() {
        // TC: FN-04
        assert_eq!(check_kv_write(0, [write("ab", None, 8)], 10), Ok(10));
    }

    #[test]
    fn refuses_write_when_it_passes_limit() {
        // TC: FN-04
        assert_eq!(
            check_kv_write(0, [write("ab", None, 9)], 10),
            Err(KvRefusal::PastQuota { after: 11, max: 10 })
        );
    }

    #[test]
    fn refuses_whole_batch_when_together_it_passes_limit() {
        // TC: FN-04
        let refused = check_kv_write(0, [write("a", None, 5), write("b", None, 5)], 10);

        assert_eq!(refused, Err(KvRefusal::PastQuota { after: 12, max: 10 }));
    }

    #[test]
    fn accepts_write_when_replacement_brings_usage_back_under_limit() {
        // TC: FN-04
        assert_eq!(check_kv_write(20, [write("a", Some(19), 1)], 10), Ok(2));
    }

    #[test]
    fn refuses_batch_when_one_key_is_too_long() {
        // TC: none a key past 256 bytes)
        let long = "k".repeat(MAX_KV_KEY_BYTES + 1);

        let refused = check_kv_write(0, [write("a", None, 1), write(&long, None, 1)], 1_000);

        assert_eq!(refused, Err(KvRefusal::KeyTooLong));
    }
}
