// Associated types: one trait, different key types per impl
//
// Define a trait `Storage` with:
//
//   type Key;
//   fn get(&self, key: &Self::Key) -> Option<String>;
//
// Then implement `Storage` for the two structs below. The whole point of
// the associated type is that EACH implementation gets to pick its own
// `Key`:
//
//   * `PhoneBook`       — `Key = String`, looks up the number by name.
//   * `Config`          — `Key = ConfigKey` (an enum), returns the
//                         setting formatted as a String.
//
// Note the return is owned `String` so callers don't have to worry
// about the lifetime of the stored value (e.g. a `u64` timeout).
//
// I AM NOT DONE

use std::collections::HashMap;

pub struct PhoneBook {
    entries: HashMap<String, String>,
}

impl PhoneBook {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }
    pub fn insert(&mut self, name: String, number: String) {
        self.entries.insert(name, number);
    }
}

pub enum ConfigKey {
    Timeout,
    Host,
    Port,
}

pub struct Config {
    timeout: u64,
    host: String,
    port: u16,
}

// TODO: define the `Storage` trait and implement it for both `PhoneBook`
// and `Config`.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phonebook_lookup() {
        let mut pb = PhoneBook::new();
        pb.insert("alice".into(), "1234".into());
        pb.insert("bob".into(), "5678".into());
        assert_eq!(pb.get(&"alice".to_string()), Some("1234".into()));
        assert_eq!(pb.get(&"bob".to_string()), Some("5678".into()));
        assert_eq!(pb.get(&"carol".to_string()), None);
    }

    #[test]
    fn config_lookup() {
        let cfg = Config {
            timeout: 30,
            host: "localhost".into(),
            port: 8080,
        };
        assert_eq!(cfg.get(&ConfigKey::Timeout), Some("30".into()));
        assert_eq!(cfg.get(&ConfigKey::Host), Some("localhost".into()));
        assert_eq!(cfg.get(&ConfigKey::Port), Some("8080".into()));
    }

    #[test]
    fn associated_type_differs_between_impls() {
        // PhoneBook::Key and Config::Key are genuinely different types,
        // so this compiles only because associated types were used.
        fn key_type_check<S: Storage>(_: &S) {}
        let pb = PhoneBook::new();
        let cfg = Config {
            timeout: 1,
            host: "h".into(),
            port: 2,
        };
        key_type_check(&pb);
        key_type_check(&cfg);
    }
}
