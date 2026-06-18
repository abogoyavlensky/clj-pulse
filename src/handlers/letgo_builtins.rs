//! let-go built-ins that have no `.lg` source: **special forms** (compiler
//! intrinsics) and, once wired, **native core functions** (Go `ns.Def`). They
//! are surfaced for hover and completion only — navigation is a deliberate
//! no-op, since there is nothing to navigate to. All use is gated by the
//! `Index::letgo_core()` marker at the call sites, so Clojure projects are
//! unaffected.

use super::letgo_native_names::NATIVE_NAMES;

/// A let-go special form (compiler intrinsic). Not a var — `resolve` cannot see
/// it — so it carries its own hand-authored description.
#[derive(Debug)]
pub struct SpecialForm {
    pub name: &'static str,
    pub usage: &'static str,
    pub doc: &'static str,
}

/// let-go's special forms, from the compiler's `specialForms` dispatch map
/// (`pkg/compiler/compiler.go`, let-go 1.10.0) plus `catch`/`finally` (parsed
/// inside `try`) and `throw`. `throw` is implemented as a native fn in let-go,
/// but Clojure documents it as a special form and it has no clojure.core entry
/// to borrow a doc from, so it is hand-authored here.
pub static SPECIAL_FORMS: &[SpecialForm] = &[
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
        name: "trace",
        usage: "(trace exprs*)",
        doc: "Evaluates the body with let-go VM instruction tracing enabled (a let-go extension).",
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
        doc: "Throws `expr` (e.g. a string or an `ex-info` map), unwinding to the nearest enclosing `try`/`catch`. (let-go implements `throw` as a native fn; Clojure documents it as a special form.)",
    },
];

/// The special form named `name`, if any.
pub fn special_form(name: &str) -> Option<&'static SpecialForm> {
    SPECIAL_FORMS.iter().find(|f| f.name == name)
}

/// Whether `name` is a let-go native core function (implemented in Go, no `.lg`
/// source). Callers borrow its doc/arglists from the clojure.core table.
/// `NATIVE_NAMES` is sorted, so a binary search suffices.
pub fn is_native(name: &str) -> bool {
    NATIVE_NAMES.binary_search(&name).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn special_form_lookup() {
        assert!(special_form("if").is_some());
        assert!(special_form("try").is_some());
        assert!(special_form("catch").is_some());
        // `throw` is hand-authored here (native in let-go, but Clojure documents
        // it as a special form and it has no clojure.core doc to borrow).
        assert!(special_form("throw").is_some());
        assert!(special_form("nope").is_none());
    }

    #[test]
    fn special_form_carries_usage_and_doc() {
        let sf = special_form("if").unwrap();
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
