// Supertraits + default methods that call each other
//
// Define two traits:
//
//   trait Identifiable {
//       fn id(&self) -> String;
//   }
//
//   trait Describable: Identifiable {          // <-- supertrait
//       /// Required: the human-friendly name.
//       fn name(&self) -> String;
//
//       /// Default body: combines `name` and the supertrait's `id`.
//       fn pretty(&self) -> String {
//           format!("{} ({})", self.name(), self.id())
//       }
//
//       /// Default body: a banner using `pretty`.
//       fn banner(&self) -> String {
//           format!("=== {} ===", self.pretty())
//       }
//   }
//
// Implement both traits for `Animal` and `Robot`:
//
//   * `Animal { species: String }`
//       - id() = species
//       - name() = species
//       - Use ALL defaults (do not override pretty/banner).
//
//   * `Robot { id: String, version: u32 }`
//       - id() = the `id` field
//       - name() = "robot#{id}@{version}"
//       - OVERRIDE pretty() to return "!{name}!" (so the default is gone,
//         but `banner` — which calls `pretty` — automatically picks up the
//         new behavior without redefining it).
//
// The lesson: default methods compose. Overriding `pretty` changes the
// output of `banner` too, because `banner` calls `self.pretty()`
// through dynamic dispatch on `self`.
//
// I AM NOT DONE

pub struct Animal {
    pub species: String,
}

pub struct Robot {
    pub id: String,
    pub version: u32,
}

// TODO: define `Identifiable` and `Describable`, then implement them for
// `Animal` and `Robot` as described above.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn animal_uses_defaults() {
        let a = Animal {
            species: "cat".into(),
        };
        assert_eq!(a.id(), "cat");
        assert_eq!(a.name(), "cat");
        // default pretty: "cat (cat)"
        assert_eq!(a.pretty(), "cat (cat)");
        // default banner wraps pretty
        assert_eq!(a.banner(), "=== cat (cat) ===");
    }

    #[test]
    fn robot_overrides_pretty_only() {
        let r = Robot {
            id: "R2".into(),
            version: 2,
        };
        assert_eq!(r.id(), "R2");
        assert_eq!(r.name(), "robot#R2@2");
        // overridden pretty
        assert_eq!(r.pretty(), "!robot#R2@2!");
        // banner uses the overridden pretty automatically
        assert_eq!(r.banner(), "=== !robot#R2@2! ===");
    }

    #[test]
    fn describable_requires_identifiable() {
        // This function only accepts things that are BOTH Describable AND
        // Identifiable — but because Describable: Identifiable, the bound
        // is satisfied by Describable alone. No need to spell both out.
        fn banner_of<D: Describable>(d: &D) -> String {
            d.banner()
        }
        let r = Robot {
            id: "X".into(),
            version: 1,
        };
        assert_eq!(banner_of(&r), "=== !robot#X@1! ===");
    }
}
