//! Language built-ins that have no source to navigate to: **special forms**
//! (compiler intrinsics — dialect-aware across Clojure and let-go) and let-go's
//! **native core functions** (Go `ns.Def`). Surfaced for hover and completion
//! only; goto-def is a deliberate no-op, since there is nothing to navigate to.
//! The active dialect is chosen by the caller via `Index::letgo_core()`.

use super::letgo_native_names::NATIVE_NAMES;

/// A special form (compiler intrinsic). Not a var — `resolve` cannot see it —
/// so it carries its own hand-authored description.
#[derive(Debug)]
pub struct SpecialForm {
    pub name: &'static str,
    pub usage: &'static str,
    pub doc: &'static str,
}

/// Special forms common to Clojure and let-go (identical usage/semantics).
/// Macros (`let`/`fn`/`loop`/`when`/`cond`/…) are intentionally absent — they
/// are real vars served by the clojure.core / `.lg` core tables, not here.
pub static COMMON_SPECIAL_FORMS: &[SpecialForm] = &[
    SpecialForm {
        name: "if",
        usage: "(if test then else?)",
        doc: "Evaluates `test`. If it is logical true (not nil or false), evaluates and yields `then`, otherwise `else` (or nil when omitted).",
    },
    SpecialForm {
        name: "do",
        usage: "(do exprs*)",
        doc: "Evaluates the expressions in order and yields the value of the last; nil when there are none.",
    },
    SpecialForm {
        name: "def",
        usage: "(def symbol doc-string? init?)",
        doc: "Interns a global var named `symbol` in the current namespace, optionally setting its root value to `init`.",
    },
    SpecialForm {
        name: "set!",
        usage: "(set! place expr)",
        doc: "Assigns the value of `expr` to a settable place (a mutable field or a thread-local dynamic var).",
    },
    SpecialForm {
        name: "fn*",
        usage: "(fn* [params*] exprs*)",
        doc: "Primitive function literal. Prefer the `fn` macro, which expands to `fn*`.",
    },
    SpecialForm {
        name: "quote",
        usage: "(quote form)",
        doc: "Yields `form` unevaluated. Reader shorthand: `'form`.",
    },
    SpecialForm {
        name: "var",
        usage: "(var symbol)",
        doc: "Yields the Var object (not its value) named by `symbol`. Reader shorthand: `#'symbol`.",
    },
    SpecialForm {
        name: "let*",
        usage: "(let* [bindings*] exprs*)",
        doc: "Primitive sequential binding form. Prefer the `let` macro, which expands to `let*`.",
    },
    SpecialForm {
        name: "loop*",
        usage: "(loop* [bindings*] exprs*)",
        doc: "Primitive `recur` target with initial bindings. Prefer the `loop` macro, which expands to `loop*`.",
    },
    SpecialForm {
        name: "recur",
        usage: "(recur exprs*)",
        doc: "Rebinds the bindings of the nearest enclosing `fn`/`loop` and jumps back to its start. Tail position only.",
    },
    SpecialForm {
        name: "try",
        usage: "(try body* (catch sym handler*)? (finally cleanup*)?)",
        doc: "Evaluates `body`; if a value is thrown, binds it to `sym` and runs the matching `catch`. A `finally` clause always runs.",
    },
    SpecialForm {
        name: "catch",
        usage: "(catch binding-sym body*)",
        doc: "Clause of `try`: binds a thrown value to `binding-sym` and handles it.",
    },
    SpecialForm {
        name: "finally",
        usage: "(finally body*)",
        doc: "Clause of `try`: its body always runs, whether or not the `try` body threw.",
    },
    SpecialForm {
        name: "throw",
        usage: "(throw expr)",
        doc: "Throws `expr` (e.g. an exception or `ex-info` map), unwinding to the nearest enclosing `try`/`catch`.",
    },
];

/// let-go-only special forms (its compiler dispatch beyond the common set).
pub static LETGO_EXTRA: &[SpecialForm] = &[SpecialForm {
    name: "trace",
    usage: "(trace exprs*)",
    doc: "Evaluates the body with let-go VM instruction tracing enabled (a let-go extension).",
}];

/// Clojure-only special forms (Java interop / locking primitives).
pub static CLOJURE_EXTRA: &[SpecialForm] = &[
    SpecialForm {
        name: ".",
        usage: "(. instance-or-Class member args*)",
        doc: "Java interop member access: calls a method or reads a field on an instance or class.",
    },
    SpecialForm {
        name: "new",
        usage: "(new Class args*)",
        doc: "Constructs a new Java object. Reader form: `(Class. args*)`.",
    },
    SpecialForm {
        name: "monitor-enter",
        usage: "(monitor-enter x)",
        doc: "Acquires the monitor lock on `x`. Low-level; prefer the `locking` macro.",
    },
    SpecialForm {
        name: "monitor-exit",
        usage: "(monitor-exit x)",
        doc: "Releases the monitor lock on `x`. Low-level; prefer the `locking` macro.",
    },
];

/// The special forms for a dialect: the common set plus the dialect's extras.
/// `letgo` selects let-go (`trace`) vs Clojure (interop/locking) extras.
pub fn special_forms(letgo: bool) -> impl Iterator<Item = &'static SpecialForm> {
    let extra = if letgo { LETGO_EXTRA } else { CLOJURE_EXTRA };
    COMMON_SPECIAL_FORMS.iter().chain(extra.iter())
}

/// The special form named `name` in the given dialect, if any.
pub fn special_form(name: &str, letgo: bool) -> Option<&'static SpecialForm> {
    special_forms(letgo).find(|f| f.name == name)
}

/// Whether `name` is a let-go native core function (implemented in Go, no `.lg`
/// source). Callers borrow its doc/arglists from the clojure.core table.
/// `NATIVE_NAMES` is sorted, so a binary search suffices.
pub fn is_native(name: &str) -> bool {
    NATIVE_NAMES.binary_search(&name).is_ok()
}

/// All native core fn names — for completion enumeration.
pub fn native_names() -> &'static [&'static str] {
    NATIVE_NAMES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_forms_resolve_in_both_dialects() {
        for letgo in [true, false] {
            assert!(special_form("if", letgo).is_some());
            assert!(special_form("try", letgo).is_some());
            assert!(special_form("catch", letgo).is_some());
            // `throw` is a special form in Clojure; let-go implements it as a
            // native fn but we present it the same way (no source either way).
            assert!(special_form("throw", letgo).is_some());
        }
        assert!(special_form("nope", true).is_none());
        assert!(special_form("nope", false).is_none());
    }

    #[test]
    fn dialect_extras_are_scoped() {
        // `trace` is let-go-only; the interop/locking forms are Clojure-only.
        assert!(special_form("trace", true).is_some());
        assert!(special_form("trace", false).is_none());
        assert!(special_form("new", false).is_some());
        assert!(special_form("new", true).is_none());

        let clojure: Vec<&str> = special_forms(false).map(|f| f.name).collect();
        assert!(clojure.contains(&"new"));
        assert!(!clojure.contains(&"trace"));
    }

    #[test]
    fn special_form_carries_usage_and_doc() {
        let sf = special_form("if", false).unwrap();
        assert!(sf.usage.contains("test"));
        assert!(!sf.doc.is_empty());
    }

    #[test]
    fn is_native_uses_generated_list() {
        // Native (Go ns.Def) clojure.core fns.
        assert!(is_native("count"));
        assert!(is_native("subs"));
        // `.lg`-defined fns are served by the live index, not this list.
        assert!(!is_native("map"));
        assert!(!is_native("definitely-not-a-builtin"));
    }
}
