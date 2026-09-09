//! End-to-end tests: spawn the real `clj-pulse` binary and speak LSP over
//! stdio with Content-Length framing, the same way VS Code/Calva drives it.
//! The client itself lives in `tests/common/mod.rs`, shared with the bench.

mod common;

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use common::*;

/// Applies LSP `TextEdit` JSON values to `source` (highest position first, so
/// earlier offsets stay valid) and returns the result.
fn apply_edits(source: &str, edits: &[Value]) -> String {
    let pos = |e: &Value| {
        (
            e["range"]["start"]["line"].as_u64().unwrap(),
            e["range"]["start"]["character"].as_u64().unwrap(),
        )
    };
    let mut ordered: Vec<&Value> = edits.iter().collect();
    ordered.sort_by_key(|e| std::cmp::Reverse(pos(e)));

    let mut text = source.to_string();
    for e in ordered {
        let start = offset_of(&text, &e["range"]["start"]);
        let end = offset_of(&text, &e["range"]["end"]);
        let new_text = e["newText"].as_str().unwrap();
        text = format!("{}{}{}", &text[..start], new_text, &text[end..]);
    }
    text
}

/// Byte offset of an LSP position (`{line, character}`) in `source`.
fn offset_of(source: &str, pos: &Value) -> usize {
    let line = pos["line"].as_u64().unwrap() as u32;
    let character = pos["character"].as_u64().unwrap() as u32;
    let (mut l, mut c) = (0u32, 0u32);
    for (i, ch) in source.char_indices() {
        if l == line && c == character {
            return i;
        }
        if ch == '\n' {
            l += 1;
            c = 0;
        } else {
            c += ch.len_utf16() as u32;
        }
    }
    source.len()
}

/// A deps.edn project whose classpath holds a JAR with two namespaces where
/// `mylib.core` requires `mylib.util` (the transitive-dependency shape). The
/// project consumer requires and uses both. Returns the tempdir (keep it alive)
/// and the canonicalized project root.
fn two_ns_jar_project() -> (tempfile::TempDir, std::path::PathBuf) {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let jar_path = root.join("mylib.jar");
    let jar_file = std::fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(jar_file);
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("mylib/util.clj", opts).unwrap();
    zip.write_all(b"(ns mylib.util)\n\n(defn helper [x] x)\n")
        .unwrap();
    zip.start_file("mylib/core.clj", opts).unwrap();
    zip.write_all(
        b"(ns mylib.core\n  (:require [mylib.util :as util]))\n\n(defn run [x] (util/helper x))\n",
    )
    .unwrap();
    zip.finish().unwrap();

    let cpcache = root.join(".cpcache");
    std::fs::create_dir_all(&cpcache).unwrap();
    std::fs::write(cpcache.join("1.cp"), jar_path.display().to_string()).unwrap();

    let consumer = root.join("src/uses_lib.clj");
    std::fs::write(
        &consumer,
        "(ns uses-lib\n  (:require [mylib.core :as core]\n            [mylib.util :as util]))\n\n(core/run 1)\n(util/helper 2)\n",
    )
    .unwrap();

    (project, root)
}

#[test]
fn test_e2e_letgo_navigation_into_lgx_deps() {
    let project = setup_named("letgo_project");
    let root = project.path().canonicalize().unwrap();
    // Hermetic: point LGX_HOME at the fixture's gitlibs tree.
    let lgx_home = root.join("lgxhome");

    let mut client = LspClient::start_with_env(&root, &[("LGX_HOME", &lgx_home)]);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");

    let app = root.join("src/app.lg");
    client.did_open(&app);

    // Into an in-workspace :local/root dep (vendor/loc).
    let (line, ch) = position_of(&app, "loc/hello");
    let loc = client.goto_definition(&app, line, ch);
    let loc_uri = loc["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("no def for loc/hello: {}", loc));
    assert!(
        loc_uri.ends_with("/vendor/loc/src/loc/core.lg"),
        "expected vendor/loc/src/loc/core.lg, got {}",
        loc_uri
    );

    // Into a git dep resolved under LGX_HOME/gitlibs.
    let (line, ch) = position_of(&app, "ext/greet");
    let ext = client.goto_definition(&app, line, ch);
    let ext_uri = ext["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("no def for ext/greet: {}", ext));
    assert!(
        ext_uri.ends_with("/gitlibs/github.com/ext/lib/DEADBEEF/src/ext/core.lg"),
        "expected the git dep core.lg, got {}",
        ext_uri
    );
}

#[test]
fn test_e2e_lint_as_navigates_to_macro_defined_name() {
    // `:lint-as {app.macros/defthing clojure.core/def}` in .clj-kondo/config.edn
    // makes `(defthing widget …)` define `widget`, so goto-def on a use of
    // `widget` resolves to the defthing form — even though `defthing` is a macro
    // from an (unindexed) dependency.
    let project = setup_named("lint_as_project");
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let core = root.join("src/app/core.clj");
    client.did_open(&core);

    // First `widget` in the file is the usage in `(defn use-it [] widget)`.
    let (use_line, use_ch) = position_of(&core, "widget");
    let resp = client.goto_definition(&core, use_line, use_ch);
    let uri = resp["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("no definition for widget: {}", resp));
    assert!(
        uri.ends_with("/src/app/core.clj"),
        "expected core.clj, got {}",
        uri
    );
    let (def_line, _) = position_of(&core, "defthing widget");
    assert_eq!(
        resp["range"]["start"]["line"].as_u64().unwrap() as u32,
        def_line,
        "definition should land on the `defthing widget` line"
    );
}

#[test]
fn test_e2e_lint_as_config_live_reload() {
    // Editing `.clj-kondo/config.edn` reloads `:lint-as` without a restart:
    // goto-def on a macro-defined name works, then stops once the mapping is
    // removed and the config change is signaled.
    let project = setup_named("lint_as_project");
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let core = root.join("src/app/core.clj");
    client.did_open(&core);

    // Initially `defthing` is lint-as'd to `def`, so `widget` navigates.
    let (use_line, use_ch) = position_of(&core, "widget");
    let before = client.goto_definition(&core, use_line, use_ch);
    assert!(
        before["uri"].is_string(),
        "widget should resolve before reload, got {}",
        before
    );

    // Drop the mapping and signal the watched-file change.
    let config = root.join(".clj-kondo/config.edn");
    std::fs::write(&config, "{}\n").unwrap();
    client.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": format!("file://{}", config.display()), "type": 2 }] }),
    );
    client.wait_for_log("config reloaded");

    // With no lint-as, `(defthing widget 1)` no longer defines `widget`.
    let after = client.goto_definition(&core, use_line, use_ch);
    assert!(
        after.is_null(),
        "widget must not resolve after the lint-as mapping is removed, got {}",
        after
    );
}

#[test]
fn test_e2e_letgo_core_navigation() {
    // A pinned let-go project (`:lg-version`) with no deps of its own: bare
    // builtins and clojure.*-aliased stdlib must navigate into the let-go core
    // source that `lgx install` fetched under LGX_HOME.
    let project = setup_named("letgo_core_project");
    let root = project.path().canonicalize().unwrap();
    let lgx_home = root.join("lgxhome");

    // Stand in for what `lgx install` fetched for version 0.0.1.
    let core = lgx_home.join("let-go/source/0.0.1/pkg/rt/core");
    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(core.join("core.lg"), "(ns core)\n(defn map [f c] c)\n").unwrap();
    std::fs::write(
        core.join("string.lg"),
        "(ns string)\n(defn join [sep c] sep)\n",
    )
    .unwrap();

    let mut client = LspClient::start_with_env(&root, &[("LGX_HOME", &lgx_home)]);
    client.initialize(&root);
    // Fires even though the project has no lgx deps — core indexing counts.
    client.wait_for_log("library indexing complete");

    let app = root.join("src/app.lg");
    client.did_open(&app);

    // Bare `map` is auto-referred from let-go's built-in core → core.lg.
    let (line, ch) = position_of(&app, "map");
    let m = client.goto_definition(&app, line, ch);
    let m_uri = m["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("no def for bare map: {}", m));
    assert!(
        m_uri.ends_with("/pkg/rt/core/core.lg"),
        "expected let-go core.lg, got {}",
        m_uri
    );

    // `str/join` through the `clojure.string` alias → the stdlib string.lg.
    let (line, ch) = position_of(&app, "str/join");
    let j = client.goto_definition(&app, line, ch);
    let j_uri = j["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("no def for str/join: {}", j));
    assert!(
        j_uri.ends_with("/pkg/rt/core/string.lg"),
        "expected let-go string.lg, got {}",
        j_uri
    );
}

#[test]
fn test_e2e_letgo_builtins_hover() {
    // let-go built-ins with no `.lg` source — special forms (`if`) and native
    // core fns (`count`) — describe themselves on hover but never navigate.
    let project = setup_named("letgo_core_project");
    let root = project.path().canonicalize().unwrap();
    let lgx_home = root.join("lgxhome");
    let core = lgx_home.join("let-go/source/0.0.1/pkg/rt/core");
    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(core.join("core.lg"), "(ns core)\n(defn map [f c] c)\n").unwrap();
    std::fs::write(
        core.join("string.lg"),
        "(ns string)\n(defn join [sep c] sep)\n",
    )
    .unwrap();

    let mut client = LspClient::start_with_env(&root, &[("LGX_HOME", &lgx_home)]);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");

    let app = root.join("src/app.lg");
    client.did_open(&app);

    // Special form `if`: hover describes it; goto-def is a no-op.
    let (line, ch) = position_of(&app, "if ");
    let h = client.hover(&app, line, ch);
    let val = h["contents"]["value"]
        .as_str()
        .unwrap_or_else(|| panic!("no hover for if: {}", h));
    assert!(val.contains("special form"), "if hover: {}", val);
    let def = client.goto_definition(&app, line, ch);
    assert!(def.is_null(), "if must not navigate, got {}", def);

    // Native core fn `count`: hover labels it native, doc borrowed from clojure.core.
    let (line, ch) = position_of(&app, "count");
    let h = client.hover(&app, line, ch);
    let val = h["contents"]["value"]
        .as_str()
        .unwrap_or_else(|| panic!("no hover for count: {}", h));
    assert!(val.contains("let-go core (native)"), "count hover: {}", val);

    // Regression: goto-def on the `str` alias declaration still resolves to the
    // stdlib namespace, even though `str` is also a native core fn name.
    let (line, ch) = position_of(&app, "str]");
    let d = client.goto_definition(&app, line, ch);
    let uri = d["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("no def for str alias: {}", d));
    assert!(
        uri.ends_with("/pkg/rt/core/string.lg"),
        "str alias should navigate to string.lg, got {}",
        uri
    );
}

#[test]
fn test_e2e_letgo_completion_harvests_native_vars_from_lang_go() {
    // A pinned let-go project: completion offers the native vars/fns this
    // version actually defines in Go, harvested from `lang.go`'s `ns.Def(...)`
    // calls — including a var like `*command-line-args*` that has no `.lg`
    // source and isn't in the static native list.
    let project = setup_named("letgo_core_project");
    let root = project.path().canonicalize().unwrap();
    let lgx_home = root.join("lgxhome");
    let rt = lgx_home.join("let-go/source/0.0.1/pkg/rt");
    let core = rt.join("core");
    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(core.join("core.lg"), "(ns core)\n(defn map [f c] c)\n").unwrap();
    // Stand in for `pkg/rt/lang.go`: the Go runtime's native defs.
    std::fs::write(
        rt.join("lang.go"),
        "func registerCore(ns *vm.Namespace) {\n\
         \tns.Def(\"count\", count)\n\
         \tns.Def(\"*command-line-args*\", vm.NIL)\n\
         }\n",
    )
    .unwrap();

    let mut client = LspClient::start_with_env(&root, &[("LGX_HOME", &lgx_home)]);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");

    let app = root.join("src/app.lg");
    client.did_open(&app);

    let last_line = std::fs::read_to_string(&app).unwrap().lines().count() as u32;
    client.did_change_insert(&app, last_line, 0, "(*comm");
    let result = client.completion_items(&app, last_line, 6);

    let items = result.as_array().expect("expected CompletionItem array");
    let item = items
        .iter()
        .find(|i| i["label"] == "*command-line-args*")
        .unwrap_or_else(|| {
            let labels: Vec<&str> = items.iter().filter_map(|i| i["label"].as_str()).collect();
            panic!("expected *command-line-args* in completions, got {labels:?}")
        });
    let detail = item["detail"].as_str().unwrap_or("");
    assert!(
        detail.contains("let-go core (native)"),
        "expected native detail, got {detail}"
    );
}

#[test]
fn test_e2e_clojure_special_form_hover() {
    // In a Clojure project, special forms (`if`) describe themselves on hover
    // and never navigate; clojure.core fns (`map`) keep their existing behavior.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();
    let mut client = LspClient::start(&root);
    client.initialize(&root);

    // A scratch file (not a committed fixture, so shared-fixture assertions in
    // other tests are untouched); did_open indexes it on the fly.
    let f = root.join("src/special_forms_demo.clj");
    std::fs::write(
        &f,
        "(ns special-forms-demo)\n\n(if true 1 2)\n(map inc [1 2])\n",
    )
    .unwrap();
    client.did_open(&f);

    // Special form `if`: hover labels it; goto-def is a no-op.
    let (line, ch) = position_of(&f, "if ");
    let h = client.hover(&f, line, ch);
    let val = h["contents"]["value"]
        .as_str()
        .unwrap_or_else(|| panic!("no hover for if: {}", h));
    assert!(val.contains("special form"), "if hover: {}", val);
    let def = client.goto_definition(&f, line, ch);
    assert!(def.is_null(), "if must not navigate, got {}", def);

    // A clojure.core fn still hovers as clojure.core (unchanged behavior).
    let (line, ch) = position_of(&f, "map ");
    let h = client.hover(&f, line, ch);
    let val = h["contents"]["value"]
        .as_str()
        .unwrap_or_else(|| panic!("no hover for map: {}", h));
    assert!(val.contains("clojure.core"), "map hover: {}", val);
}

#[test]
fn test_e2e_no_diagnostics_on_lgx_edn() {
    // Opening lgx.edn must not flag dependency coordinates (`my/loc`,
    // `ext/lib`) as unresolved namespaces — EDN config files are not source.
    let project = setup_named("letgo_project");
    let root = project.path().canonicalize().unwrap();
    let lgx_home = root.join("lgxhome");

    let mut client = LspClient::start_with_env(&root, &[("LGX_HOME", &lgx_home)]);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");

    let lgx = root.join("lgx.edn");
    client.did_open(&lgx);

    let diags = client.wait_for_diagnostics("/lgx.edn");
    let list = diags["diagnostics"].as_array().expect("diagnostics array");
    assert!(
        list.is_empty(),
        "expected no diagnostics on lgx.edn, got {}",
        diags["diagnostics"]
    );
}

#[test]
fn test_e2e_definition_on_protocol_method() {
    // Go-to-definition on a protocol-method call lands on the method's
    // signature inside the defprotocol.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let proto = root.join("src/proto.clj");
    std::fs::write(
        &proto,
        "(ns proto)\n(defprotocol Storage\n  (fetch [this id]))\n\n(defn run [s] (fetch s 1))\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("Indexed");
    client.did_open(&proto);

    let (uline, uch) = position_of(&proto, "fetch s");
    let result = client.goto_definition(&proto, uline, uch);
    let uri = result["uri"].as_str().expect("expected Location");
    assert!(uri.ends_with("/src/proto.clj"), "got {}", uri);

    let (decl_line, _) = position_of(&proto, "fetch [this");
    assert_eq!(result["range"]["start"]["line"], json!(decl_line));
}

#[test]
fn test_e2e_definition_on_record_factory() {
    // Go-to-definition on the auto-generated `map->DB` / `->DB` factory fns
    // lands on the `defrecord`.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let recs = root.join("src/recs.clj");
    std::fs::write(
        &recs,
        "(ns recs)\n(defrecord DB [conn])\n\n(defn make [c] (map->DB {:conn c}))\n(defn make2 [c] (->DB c))\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("Indexed");
    client.did_open(&recs);

    let (decl_line, _) = position_of(&recs, "defrecord DB");

    for usage in ["map->DB {", "->DB c"] {
        let (l, c) = position_of(&recs, usage);
        let result = client.goto_definition(&recs, l, c);
        let uri = result["uri"]
            .as_str()
            .unwrap_or_else(|| panic!("no def for {}: {}", usage, result));
        assert!(uri.ends_with("/src/recs.clj"), "{} -> {}", usage, uri);
        assert_eq!(
            result["range"]["start"]["line"],
            json!(decl_line),
            "{} did not navigate to the defrecord",
            usage
        );
    }
}

#[test]
fn test_e2e_definition_on_protocol_method_impl() {
    // A protocol method *implementation* navigates to the protocol's
    // *declaration* in another namespace — exercises the occurrence fallback,
    // since `resolve_symbol` can't resolve the bare impl name across namespaces.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let proto = root.join("src/proto.clj");
    std::fs::write(
        &proto,
        "(ns app.proto)\n(defprotocol Worker\n  (run-task [this job]))\n",
    )
    .unwrap();
    let impl_file = root.join("src/impl.clj");
    std::fs::write(
        &impl_file,
        "(ns app.impl\n  (:require [app.proto :as p]))\n(defrecord Runner [id]\n  p/Worker\n  (run-task [this job] job))\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("Indexed");
    client.did_open(&impl_file);

    let (line, ch) = position_of(&impl_file, "run-task");
    let result = client.goto_definition(&impl_file, line, ch);
    let uri = result["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("no def for run-task impl: {}", result));
    assert!(uri.ends_with("/src/proto.clj"), "got {}", uri);

    let (decl_line, _) = position_of(&proto, "run-task");
    assert_eq!(result["range"]["start"]["line"], json!(decl_line));
}

#[test]
fn test_e2e_definition_on_defmethod() {
    // goto-def on a `defmethod` head navigates to the `defmulti` declaration in
    // another namespace — the multimethod analog of the protocol-impl case.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let def = root.join("src/multi_def.clj");
    std::fs::write(&def, "(ns app.multi)\n(defmulti area :kind)\n").unwrap();
    let impl_file = root.join("src/multi_impl.clj");
    std::fs::write(
        &impl_file,
        "(ns app.impl\n  (:require [app.multi :as m]))\n(defmethod m/area :circle [x] (:r x))\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("Indexed");
    client.did_open(&impl_file);

    let (line, ch) = position_of(&impl_file, "m/area");
    let result = client.goto_definition(&impl_file, line, ch);
    let uri = result["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("no def for m/area: {}", result));
    assert!(uri.ends_with("/src/multi_def.clj"), "got {}", uri);

    let (decl_line, _) = position_of(&def, "area");
    assert_eq!(result["range"]["start"]["line"], json!(decl_line));
}

#[test]
fn test_e2e_definition_on_defmethod_letgo() {
    // Same as above for a let-go (`.lg`) project — the fix is dialect-agnostic.
    let project = setup_named("letgo_core_project");
    let root = project.path().canonicalize().unwrap();
    // Hermetic: empty LGX_HOME so no real let-go core is indexed.
    let lgx_home = root.join("lgxhome");

    let def = root.join("src/mdef.lg");
    std::fs::write(&def, "(ns mdef)\n(defmulti area :kind)\n").unwrap();
    let impl_file = root.join("src/mimpl.lg");
    std::fs::write(
        &impl_file,
        "(ns mimpl\n  (:require [mdef :as m]))\n(defmethod m/area :circle [x] (:r x))\n",
    )
    .unwrap();

    let mut client = LspClient::start_with_env(&root, &[("LGX_HOME", &lgx_home)]);
    client.initialize(&root);
    client.wait_for_log("Indexed");
    client.did_open(&impl_file);

    let (line, ch) = position_of(&impl_file, "m/area");
    let result = client.goto_definition(&impl_file, line, ch);
    let uri = result["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("no def for m/area (lg): {}", result));
    assert!(uri.ends_with("/src/mdef.lg"), "got {}", uri);

    let (decl_line, _) = position_of(&def, "area");
    assert_eq!(result["range"]["start"]["line"], json!(decl_line));
}

#[test]
fn test_e2e_protocol_impl_wins_over_colliding_def() {
    // A same-namespace defn shares the impl method's name. Go-to-definition on
    // the *impl* must reach the protocol declaration (the position-specific
    // occurrence), not the colliding local var.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let proto = root.join("src/proto.clj");
    std::fs::write(
        &proto,
        "(ns app.proto)\n(defprotocol Worker\n  (run-task [this job]))\n",
    )
    .unwrap();
    let impl_file = root.join("src/impl.clj");
    std::fs::write(
        &impl_file,
        "(ns app.impl\n  (:require [app.proto :as p]))\n(defn run-task [x] x)\n(defrecord Runner [id]\n  p/Worker\n  (run-task [this job] job))\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("Indexed");
    client.did_open(&impl_file);

    // Target the impl head specifically (`run-task [this …]`), not the defn.
    let (line, ch) = position_of(&impl_file, "run-task [this");
    let result = client.goto_definition(&impl_file, line, ch);
    let uri = result["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("no def for colliding impl: {}", result));
    assert!(
        uri.ends_with("/src/proto.clj"),
        "impl should reach the protocol decl, not the local defn; got {}",
        uri
    );
    let (decl_line, _) = position_of(&proto, "run-task");
    assert_eq!(result["range"]["start"]["line"], json!(decl_line));
}

#[test]
fn test_e2e_cross_file_definition() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let utils = root.join("src/utils.clj");
    let core = root.join("src/core.clj");
    client.did_open(&utils);

    let (line, ch) = position_of(&utils, "core/add");
    let result = client.goto_definition(&utils, line, ch);

    assert!(
        !result.is_null(),
        "goto-definition on core/add returned null"
    );
    let uri = result["uri"].as_str().expect("expected Location");
    assert!(
        uri.ends_with("/src/core.clj"),
        "expected core.clj, got {}",
        uri
    );
    let (def_line, _) = position_of(&core, "defn add");
    assert_eq!(result["range"]["start"]["line"], json!(def_line));
}

#[test]
fn test_e2e_goto_definition_local_in_let() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let locals = root.join("src/locals.clj");
    client.did_open(&locals);
    let text = std::fs::read_to_string(&locals).unwrap();

    // Binding sites are the first occurrence of each name.
    let base_def = start_of(&text, "base");
    let scaled_def = start_of(&text, "scaled");

    // The reported bug: goto-def on `base` used in the *later* binding
    // `(* base 2)` must land on the `base` binding site in the same file.
    let (line, ch) = position_of(&locals, "base 2");
    let r = client.goto_definition(&locals, line, ch);
    assert!(!r.is_null(), "no definition for local `base`: {}", r);
    assert!(
        r["uri"].as_str().unwrap().ends_with("/src/locals.clj"),
        "expected same file, got {}",
        r
    );
    assert_eq!(r["range"]["start"]["line"], json!(base_def.0));
    assert_eq!(r["range"]["start"]["character"], json!(base_def.1));

    // And goto-def on `scaled` used in the body lands on its binding site.
    let (line, ch) = position_of(&locals, "base scaled");
    let r = client.goto_definition(&locals, line, ch);
    assert_eq!(r["range"]["start"]["line"], json!(scaled_def.0));
    assert_eq!(r["range"]["start"]["character"], json!(scaled_def.1));
}

#[test]
fn test_e2e_completion_local_in_let() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let locals = root.join("src/locals.clj");
    client.did_open(&locals);
    let text = std::fs::read_to_string(&locals).unwrap();

    // Type a partial local reference `ba` inside the let body, where `base`
    // and `scaled` are in scope.
    let (bl, bc) = start_of(&text, "(+ base");
    client.did_change_insert(&locals, bl, bc, "ba");
    let result = client.completion_items(&locals, bl, bc + 2);

    let items = result.as_array().expect("expected CompletionItem array");
    let base = items
        .iter()
        .find(|i| i["label"] == json!("base"))
        .unwrap_or_else(|| panic!("expected `base` local in completions: {:?}", items));
    assert_eq!(base["detail"], json!("local"), "base item: {}", base);
}

#[test]
fn test_e2e_references_local_in_let() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let locals = root.join("src/locals.clj");
    client.did_open(&locals);

    // `base` is bound once and used twice (in the `scaled` binding and the
    // body) → declaration + 2 usages = 3 locations, all in this file.
    let (line, ch) = position_of(&locals, "base"); // first occurrence: the binding
    let result = client.references(&locals, line, ch, true);
    let locs = result
        .as_array()
        .unwrap_or_else(|| panic!("references returned null for local `base`: {}", result));
    assert_eq!(locs.len(), 3, "decl + 2 usages: {:?}", locs);
    assert!(
        locs.iter()
            .all(|l| l["uri"].as_str().unwrap().ends_with("/src/locals.clj")),
        "all references in locals.clj: {:?}",
        locs
    );

    // Without the declaration, only the two usages are returned.
    let result = client.references(&locals, line, ch, false);
    assert_eq!(
        result.as_array().unwrap().len(),
        2,
        "two usages: {}",
        result
    );
}

#[test]
fn test_e2e_definition_from_file_outside_source_paths() {
    // deps.edn has :paths ["src"], so dev/scratch.clj is NOT indexed at
    // startup — but navigation from an opened file must still work.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();
    let scratch = root.join("dev/scratch.clj");
    std::fs::create_dir_all(scratch.parent().unwrap()).unwrap();
    std::fs::write(
        &scratch,
        "(ns scratch\n  (:require [simple.core :as core]))\n\n(core/add 1 2)\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.did_open(&scratch);

    let (line, ch) = position_of(&scratch, "core/add");
    let result = client.goto_definition(&scratch, line, ch);

    assert!(
        !result.is_null(),
        "goto-definition from a file outside :paths returned null"
    );
    let uri = result["uri"].as_str().expect("expected Location");
    assert!(
        uri.ends_with("/src/core.clj"),
        "expected core.clj, got {}",
        uri
    );
}

#[test]
fn test_e2e_hover_shows_doc() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let utils = root.join("src/utils.clj");
    client.did_open(&utils);

    let (line, ch) = position_of(&utils, "core/add");
    let result = client.hover(&utils, line, ch);

    assert!(!result.is_null(), "hover returned null");
    let value = result["contents"]["value"].as_str().unwrap();
    assert!(value.contains("```clojure"), "no code block: {}", value);
    assert!(value.contains("Adds two numbers."), "no doc: {}", value);
}

/// Builds a hermetic JDK `src.zip` from `(entry, java-source)` pairs.
fn make_jdk_src_zip(entries: &[(&str, &str)]) -> tempfile::NamedTempFile {
    let tmp = tempfile::Builder::new().suffix(".zip").tempfile().unwrap();
    let file = std::fs::File::create(tmp.path()).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default();
    for (name, content) in entries {
        zip.start_file(*name, opts).unwrap();
        zip.write_all(content.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
    tmp
}

#[test]
fn test_e2e_java_definition_and_hover() {
    // Hermetic JDK source pointed at by CLJ_PULSE_JDK_SRC, so the test never
    // depends on the box's JDK. `demo.lib.Greeter` is imported; `java.lang.Sample`
    // resolves without an import (auto-`java.lang`).
    let src_zip = make_jdk_src_zip(&[
        (
            "java.base/demo/lib/Greeter.java",
            "package demo.lib;\n/** A greeter. */\npublic class Greeter {\n  \
             /** Greet by name. */\n  public static String greet(String name) { return name; }\n}\n",
        ),
        (
            "java.base/java/lang/Sample.java",
            "package java.lang;\npublic class Sample {\n  \
             public static Sample of(long n) { return null; }\n}\n",
        ),
    ]);

    let project = setup_project();
    let root = project.path().canonicalize().unwrap();
    let probe = root.join("src/javaprobe.clj");
    std::fs::write(
        &probe,
        "(ns simple.javaprobe\n  (:import [demo.lib Greeter]))\n\n\
         (defn g [n] (Greeter/greet n))\n\n(defn s [] (Sample/of 1))\n",
    )
    .unwrap();

    let mut client = LspClient::start_with_env(&root, &[("CLJ_PULSE_JDK_SRC", src_zip.path())]);
    client.initialize(&root);
    client.wait_for_log("JDK source indexed");
    client.did_open(&probe);

    // Static-member navigation lands in the src.zip Greeter.java.
    let (line, ch) = position_of(&probe, "Greeter/greet");
    let def = client.goto_definition(&probe, line, ch);
    let uri = def["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("expected Location, got {def}"));
    assert!(
        uri.contains(".zip!/") && uri.ends_with("Greeter.java"),
        "expected src.zip Greeter.java, got {uri}"
    );

    // Hover shows the Java signature and Javadoc.
    let hov = client.hover(&probe, line, ch);
    let value = hov["contents"]["value"]
        .as_str()
        .unwrap_or_else(|| panic!("hover null: {hov}"));
    assert!(
        value.contains("greet(String name)"),
        "signature missing: {value}"
    );
    assert!(value.contains("Greet by name"), "javadoc missing: {value}");

    // Auto-imported java.lang resolves without an explicit :import.
    let (sline, sch) = position_of(&probe, "Sample/of");
    let sdef = client.goto_definition(&probe, sline, sch);
    let suri = sdef["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("expected Location, got {sdef}"));
    assert!(
        suri.ends_with("Sample.java"),
        "expected Sample.java, got {suri}"
    );
}

#[test]
fn test_e2e_java_completion_and_signature() {
    let src_zip = make_jdk_src_zip(&[(
        "java.base/demo/lib/Greeter.java",
        "package demo.lib;\npublic class Greeter {\n  public Greeter(int seed) {}\n  \
         public static String greet(String name) { return name; }\n}\n",
    )]);

    let project = setup_project();
    let root = project.path().canonicalize().unwrap();
    let probe = root.join("src/jcomp.clj");
    std::fs::write(
        &probe,
        "(ns simple.jcomp\n  (:import [demo.lib Greeter]))\n",
    )
    .unwrap();

    let mut client = LspClient::start_with_env(&root, &[("CLJ_PULSE_JDK_SRC", src_zip.path())]);
    client.initialize(&root);
    client.wait_for_log("JDK source indexed");
    client.did_open(&probe);

    let base = std::fs::read_to_string(&probe).unwrap().lines().count() as u32;

    // Static-member completion: `Greeter/g` → `Greeter/greet` (labelled with the
    // class prefix so the editor's `Class/...` filter keeps it).
    client.did_change_insert(&probe, base, 0, "Greeter/g\n");
    let comp = client.completion_items(&probe, base, 9);
    let labels: Vec<&str> = comp
        .as_array()
        .expect("completion array")
        .iter()
        .filter_map(|i| i["label"].as_str())
        .collect();
    assert!(
        labels.contains(&"Greeter/greet"),
        "static-member completion: {labels:?}"
    );

    // Class-name completion: PascalCase `Gr` → Greeter.
    client.did_change_insert(&probe, base + 1, 0, "Gr\n");
    let comp2 = client.completion_items(&probe, base + 1, 2);
    let labels2: Vec<&str> = comp2
        .as_array()
        .expect("completion array")
        .iter()
        .filter_map(|i| i["label"].as_str())
        .collect();
    assert!(
        labels2.contains(&"Greeter"),
        "class-name completion: {labels2:?}"
    );

    // Signature help: `(Greeter/greet ` → greet(String name).
    client.did_change_insert(&probe, base + 2, 0, "(Greeter/greet \n");
    let sig = client.signature_help(&probe, base + 2, 15);
    assert!(!sig.is_null(), "no signature help");
    let label = sig["signatures"][0]["label"].as_str().unwrap_or("");
    assert!(label.contains("greet(String name)"), "signature: {label}");
}

#[test]
#[ignore = "needs a real JDK with lib/src.zip discoverable on the machine"]
fn test_e2e_java_real_jdk_discovery() {
    // No CLJ_PULSE_JDK_SRC override: exercises *real* discovery (JAVA_HOME, then a
    // `java -XshowSettings` probe) against the machine's JDK. Covers the reported
    // cases: `Thread/sleep` (auto-`java.lang`) and an imported `java.security` class.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();
    let probe = root.join("src/jreal.clj");
    std::fs::write(
        &probe,
        "(ns simple.jreal\n  (:import (java.security MessageDigest)))\n\n\
         (defn nap [] (Thread/sleep 10))\n\n(defn dig [] (MessageDigest/getInstance \"MD5\"))\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root); // inherits env (JAVA_HOME/PATH)
    client.initialize(&root);
    client.wait_for_log("JDK source indexed");
    client.did_open(&probe);

    let (tl, tc) = position_of(&probe, "Thread/sleep");
    let tdef = client.goto_definition(&probe, tl, tc);
    let turi = tdef["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("Thread/sleep: expected Location, got {tdef}"));
    assert!(
        turi.contains(".zip!/") && turi.ends_with("Thread.java"),
        "expected JDK src.zip Thread.java, got {turi}"
    );

    let (ml, mc) = position_of(&probe, "MessageDigest/getInstance");
    let mdef = client.goto_definition(&probe, ml, mc);
    let muri = mdef["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("MessageDigest: expected Location, got {mdef}"));
    assert!(
        muri.ends_with("MessageDigest.java"),
        "expected MessageDigest.java, got {muri}"
    );
}

#[test]
fn test_e2e_completion_with_alias_prefix() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let utils = root.join("src/utils.clj");
    client.did_open(&utils);

    let (line, ch) = position_of(&utils, "core/add");
    let result = client.completion_items(&utils, line, ch);

    assert!(!result.is_null(), "completion returned null");
    let labels: Vec<&str> = result
        .as_array()
        .expect("expected CompletionItem array")
        .iter()
        .filter_map(|i| i["label"].as_str())
        .collect();
    assert!(
        labels.contains(&"core/add"),
        "expected core/add in completions, got {:?}",
        labels
    );
}

#[test]
fn test_e2e_completion_list_is_incomplete() {
    // Fuzzy tiers and the namespace cap mean a longer prefix can yield
    // candidates the current list lacks, so the server must not let the client
    // filter its cache instead of asking again.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let utils = root.join("src/utils.clj");
    client.did_open(&utils);

    let (line, ch) = position_of(&utils, "core/add");
    let result = client.completion(&utils, line, ch);

    assert_eq!(
        result["isIncomplete"], true,
        "completion list must be incomplete: {}",
        result
    );
    assert!(
        result["items"].is_array(),
        "completion items missing: {}",
        result
    );
}

#[test]
fn test_e2e_completion_capabilities() {
    // `/` retriggers completion after an alias, `:` opens keyword completion,
    // and documentation is fetched per item — all three have to reach the
    // client through `initialize`.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    let result = client.initialize(&root);

    let provider = &result["capabilities"]["completionProvider"];
    assert_eq!(
        provider["triggerCharacters"],
        serde_json::json!(["/", ":"]),
        "completion trigger characters: {}",
        provider
    );
    assert_eq!(
        provider["resolveProvider"], true,
        "completion resolve provider: {}",
        provider
    );
}

#[test]
fn test_e2e_completion_after_slash_trigger() {
    // Typing the trigger character: the cursor sits right after `core/`, so the
    // name prefix is empty and the whole namespace is offered.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let utils = root.join("src/utils.clj");
    client.did_open(&utils);

    // Type the alias and the trigger character, nothing more.
    let last_line = std::fs::read_to_string(&utils).unwrap().lines().count() as u32;
    client.did_change_insert(&utils, last_line, 0, "(core/");
    let items = client.completion_items(&utils, last_line, 6);

    let labels: Vec<&str> = items
        .as_array()
        .expect("expected CompletionItem array")
        .iter()
        .filter_map(|i| i["label"].as_str())
        .collect();
    assert!(
        labels.contains(&"core/add"),
        "expected core/add right after the slash, got {:?}",
        labels
    );
}

#[test]
fn test_e2e_completion_resolve_adds_documentation() {
    // Items travel without documentation; the client asks for it per item.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let utils = root.join("src/utils.clj");
    client.did_open(&utils);

    let (line, ch) = position_of(&utils, "core/add");
    let items = client.completion_items(&utils, line, ch);
    let item = items
        .as_array()
        .expect("completion items")
        .iter()
        .find(|i| i["label"] == "core/add")
        .unwrap_or_else(|| panic!("core/add not offered: {}", items))
        .clone();
    assert!(
        item["documentation"].is_null(),
        "documentation sent up front: {}",
        item
    );

    let resolved = client.completion_resolve(item);
    let doc = resolved["documentation"]["value"]
        .as_str()
        .unwrap_or_else(|| panic!("no documentation after resolve: {}", resolved));
    assert!(
        doc.contains("Adds two numbers"),
        "unexpected documentation: {}",
        doc
    );
}

#[test]
fn test_e2e_completion_bare_prefix_in_current_ns() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let utils = root.join("src/utils.clj");
    client.did_open(&utils);

    // Type a partial bare symbol and complete it
    let last_line = std::fs::read_to_string(&utils).unwrap().lines().count() as u32;
    client.did_change_insert(&utils, last_line, 0, "(add-an");
    let result = client.completion_items(&utils, last_line, 7);

    let labels: Vec<&str> = result
        .as_array()
        .expect("expected CompletionItem array")
        .iter()
        .filter_map(|i| i["label"].as_str())
        .collect();
    assert!(
        labels.contains(&"add-and-double"),
        "expected add-and-double in completions, got {:?}",
        labels
    );
}

#[test]
fn test_e2e_completion_clojure_core_builtins() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let utils = root.join("src/utils.clj");
    client.did_open(&utils);

    let last_line = std::fs::read_to_string(&utils).unwrap().lines().count() as u32;
    client.did_change_insert(&utils, last_line, 0, "(redu");
    let result = client.completion_items(&utils, last_line, 5);

    let items = result.as_array().expect("expected CompletionItem array");
    let labels: Vec<&str> = items.iter().filter_map(|i| i["label"].as_str()).collect();
    assert!(
        labels.contains(&"reduce") && labels.contains(&"reduce-kv"),
        "expected reduce/reduce-kv in completions, got {:?}",
        labels
    );
    let reduce = items.iter().find(|i| i["label"] == "reduce").unwrap();
    let detail = reduce["detail"].as_str().unwrap();
    assert!(
        detail.starts_with("clojure.core"),
        "expected clojure.core detail, got {}",
        detail
    );
}

#[test]
fn test_e2e_completion_from_jar_library() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let jar_path = root.join("mylib.jar");
    let jar_file = std::fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(jar_file);
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("mylib/util.clj", opts).unwrap();
    zip.write_all(b"(ns mylib.util)\n\n(defn helper\n  \"Does helping.\"\n  [x]\n  x)\n\n(defn helper-two [x] x)\n")
        .unwrap();
    zip.finish().unwrap();

    let cpcache = root.join(".cpcache");
    std::fs::create_dir_all(&cpcache).unwrap();
    std::fs::write(cpcache.join("1.cp"), jar_path.display().to_string()).unwrap();

    let consumer = root.join("src/uses_lib.clj");
    std::fs::write(
        &consumer,
        "(ns uses-lib\n  (:require [mylib.util :as u]))\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");
    client.did_open(&consumer);

    client.did_change_insert(&consumer, 2, 0, "(u/hel");
    let result = client.completion_items(&consumer, 2, 6);

    let labels: Vec<&str> = result
        .as_array()
        .expect("expected CompletionItem array")
        .iter()
        .filter_map(|i| i["label"].as_str())
        .collect();
    assert!(
        labels.contains(&"u/helper") && labels.contains(&"u/helper-two"),
        "expected u/helper completions from JAR lib, got {:?}",
        labels
    );
}

/// Writes `test/simple/core_test.clj` into a temp project copy, pulling
/// `clojure.test` in with `refer_clause`. Written at runtime rather than
/// committed into the `simple_project` fixture: `test` is a conventional source
/// root (see `config::source_paths`), so a committed copy would be indexed for
/// every test in this file and its `core/add` usages would show up in the
/// unrelated references / rename / workspace-symbol assertions.
fn write_core_test(root: &Path, refer_clause: &str) -> std::path::PathBuf {
    let dir = root.join("test/simple");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("core_test.clj");
    std::fs::write(
        &file,
        format!(
            "(ns simple.core-test\n  (:require [clojure.test {}]\n                         [simple.core :as core]))\n\n             (deftest add-works\n  (testing \"adds\"\n    (is (= 3 (core/add 1 2)))))\n\n             (deftest multiply-works\n  (is (= 6 (core/multiply 2 3))))\n",
            refer_clause
        ),
    )
    .unwrap();
    file
}

/// Writes a fake `clojure.test` JAR into `root` and points `.cpcache` at it, so
/// the library index carries the macros a test file refers.
fn write_clojure_test_jar(root: &Path) {
    let jar_path = root.join("clojure-test.jar");
    let jar_file = std::fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(jar_file);
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("clojure/test.clj", opts).unwrap();
    zip.write_all(
        b"(ns clojure.test)\n\n          (defmacro deftest\n  \"Defines a test.\"\n  [name & body]\n  nil)\n\n          (defmacro deftest- [name & body] nil)\n\n          (defmacro is [form] nil)\n\n          (defmacro testing [s & body] nil)\n",
    )
    .unwrap();
    zip.finish().unwrap();

    let cpcache = root.join(".cpcache");
    std::fs::create_dir_all(&cpcache).unwrap();
    std::fs::write(cpcache.join("1.cp"), jar_path.display().to_string()).unwrap();
}

#[test]
fn test_e2e_deftest_outline_and_completion() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();
    write_clojure_test_jar(&root);
    let test_file = write_core_test(&root, ":refer [deftest is testing]");

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");
    client.did_open(&test_file);

    // Outline: both deftests, as functions (kind 12), selection on the name.
    let symbols = client.document_symbols(&test_file);
    let list = symbols.as_array().expect("DocumentSymbol array");
    let names: Vec<&str> = list.iter().filter_map(|s| s["name"].as_str()).collect();
    assert_eq!(names, vec!["add-works", "multiply-works"], "{symbols}");
    for sym in list {
        assert_eq!(sym["kind"], json!(12), "deftest is a function: {symbols}");
    }
    let text = std::fs::read_to_string(&test_file).unwrap();
    let (name_line, name_col) = start_of(&text, "add-works");
    let selection = &list[0]["selectionRange"];
    assert_eq!(selection["start"]["line"], json!(name_line), "{symbols}");
    assert_eq!(
        selection["start"]["character"],
        json!(name_col),
        "{symbols}"
    );
    assert_eq!(
        selection["end"]["character"],
        json!(name_col + "add-works".len() as u32),
        "{symbols}"
    );

    // Workspace search (Cmd+T) finds the test by name.
    let found = client.workspace_symbols("add-works");
    let hits = found.as_array().expect("SymbolInformation array");
    assert_eq!(hits[0]["name"], json!("add-works"), "{found}");
    assert_eq!(hits[0]["containerName"], json!("simple.core-test"));

    // References on the test name: the definition only, never a self-usage.
    let refs = client.references(&test_file, name_line, name_col + 1, true);
    let locations = refs.as_array().expect("Location array");
    assert_eq!(locations.len(), 1, "expected only the definition: {refs}");

    // Completion of a fresh `(deft` offers the referred macros.
    let last_line = text.lines().count() as u32;
    client.did_change_insert(&test_file, last_line, 0, "(deft");
    let result = client.completion_items(&test_file, last_line, 5);
    let labels: Vec<&str> = result
        .as_array()
        .expect("CompletionItem array")
        .iter()
        .filter_map(|i| i["label"].as_str())
        .collect();
    // Only what the refer vector names: `deftest-` is not in scope here.
    assert!(
        labels.contains(&"deftest"),
        "expected deftest completion, got {:?}",
        labels
    );
    assert!(
        !labels.contains(&"deftest-"),
        "deftest- is not referred, got {:?}",
        labels
    );
}

#[test]
fn test_e2e_deftest_refer_all_completion() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();
    write_clojure_test_jar(&root);
    // Same tests, pulled in with `:refer :all` instead of a refer vector.
    let test_file = write_core_test(&root, ":refer :all");
    let text = std::fs::read_to_string(&test_file).unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");
    client.did_open(&test_file);

    // The deftests are still symbols, resolved through the refer-all namespace.
    let symbols = client.document_symbols(&test_file);
    let names: Vec<&str> = symbols
        .as_array()
        .expect("DocumentSymbol array")
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert_eq!(names, vec!["add-works", "multiply-works"], "{symbols}");

    // Hover on the bare `is` — not on the `=` beside it — resolves through the
    // refer-all fallback to the macro in the fake clojure.test JAR.
    let (is_line, is_col) = start_of(&text, "(is (= 3");
    let hover = client.hover(&test_file, is_line, is_col + 1);
    let shown = hover["contents"]["value"]
        .as_str()
        .unwrap_or_else(|| panic!("no hover for refer-all `is`: {hover}"));
    assert!(
        shown.contains("defmacro is") && shown.contains("clojure.test"),
        "hover did not resolve `is` through :refer :all: {hover}"
    );

    let last_line = text.lines().count() as u32;
    client.did_change_insert(&test_file, last_line, 0, "(deft");
    let result = client.completion_items(&test_file, last_line, 5);
    let labels: Vec<&str> = result
        .as_array()
        .expect("CompletionItem array")
        .iter()
        .filter_map(|i| i["label"].as_str())
        .collect();
    // `:refer :all` puts every public macro of the namespace in scope, so the
    // whole `deft…` family is offered, not just what a refer vector named.
    assert!(
        labels.contains(&"deftest") && labels.contains(&"deftest-"),
        "expected deftest completions from :refer :all, got {:?}",
        labels
    );
}

#[test]
fn test_e2e_no_diagnostics_on_project_clj() {
    // Opening project.clj must not flag dependency coordinates
    // (`org.clojure/clojure`, `ring/ring-defaults`) as unresolved namespaces —
    // it is a build manifest, not source (like deps.edn / lgx.edn).
    let project = setup_named("lein_project");
    let root = project.path().canonicalize().unwrap();

    let project_clj = root.join("project.clj");
    std::fs::write(
        &project_clj,
        "(defproject app \"0.1.0\"\n  :dependencies [[org.clojure/clojure \"1.11.1\"]\n                 [ring/ring-defaults \"0.3.2\"]])\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.did_open(&project_clj);

    let diags = client.wait_for_diagnostics("/project.clj");
    let list = diags["diagnostics"].as_array().expect("diagnostics array");
    assert!(
        list.is_empty(),
        "expected no diagnostics on project.clj, got {}",
        diags["diagnostics"]
    );
}

#[test]
fn test_e2e_leiningen_navigation_into_m2_jar() {
    // A Leiningen project with no .cpcache: deps are read from project.clj and
    // mapped to JARs under its :local-repo Maven tree. The fixture's
    // project.clj also carries `^{:protect false}` metadata and a `#"user"`
    // regex, proving the masked parser resolves :dependencies regardless.
    let project = setup_named("lein_project");
    let root = project.path().canonicalize().unwrap();

    // Lay down the declared dep [mylib "1.0.0"] at its Maven coordinate inside
    // the hermetic :local-repo (<root>/m2).
    let jar_path = root.join("m2/mylib/mylib/1.0.0/mylib-1.0.0.jar");
    std::fs::create_dir_all(jar_path.parent().unwrap()).unwrap();
    let jar_file = std::fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(jar_file);
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("mylib/util.clj", opts).unwrap();
    zip.write_all(b"(ns mylib.util)\n\n(defn helper\n  \"Does helping.\"\n  [x]\n  x)\n\n(defn helper-two [x] x)\n")
        .unwrap();
    zip.finish().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");

    let consumer = root.join("src/uses_lib.clj");
    client.did_open(&consumer);

    client.did_change_insert(&consumer, 2, 0, "(u/hel");
    let result = client.completion_items(&consumer, 2, 6);

    let labels: Vec<&str> = result
        .as_array()
        .expect("expected CompletionItem array")
        .iter()
        .filter_map(|i| i["label"].as_str())
        .collect();
    assert!(
        labels.contains(&"u/helper") && labels.contains(&"u/helper-two"),
        "expected u/helper completions from project.clj-resolved JAR, got {:?}",
        labels
    );
}

#[test]
fn test_e2e_project_clj_change_indexes_new_deps() {
    // Editing project.clj while the server runs must re-resolve Leiningen deps
    // (it is a manifest, like deps.edn), not merely re-index it as source.
    let project = setup_named("lein_project");
    let root = project.path().canonicalize().unwrap();

    // The dep JAR exists on disk, but project.clj initially declares nothing.
    let jar_path = root.join("m2/mylib/mylib/1.0.0/mylib-1.0.0.jar");
    std::fs::create_dir_all(jar_path.parent().unwrap()).unwrap();
    let jar_file = std::fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(jar_file);
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("mylib/util.clj", opts).unwrap();
    zip.write_all(b"(ns mylib.util)\n(defn helper [x] x)\n")
        .unwrap();
    zip.finish().unwrap();

    let project_clj = root.join("project.clj");
    std::fs::write(
        &project_clj,
        "(defproject lein-app \"0.1.0\" :local-repo \"m2\" :source-paths [\"src\"])\n",
    )
    .unwrap();

    let consumer = root.join("src/uses_lib.clj");
    std::fs::write(
        &consumer,
        "(ns uses-lib\n  (:require [mylib.util :as u]))\n\n(u/helper 42)\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    // No deps yet, so the library task logs a warning rather than completion;
    // sync on the project index instead.
    client.wait_for_log("Indexed");
    client.did_open(&consumer);

    let (line, ch) = position_of(&consumer, "u/helper");
    assert!(
        client.goto_definition(&consumer, line, ch).is_null(),
        "lib resolved before being declared in project.clj"
    );

    // Declare the dependency and signal the manifest change.
    std::fs::write(
        &project_clj,
        "(defproject lein-app \"0.1.0\" :local-repo \"m2\"\n  :dependencies [[mylib \"1.0.0\"]]\n  :source-paths [\"src\"])\n",
    )
    .unwrap();
    client.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": format!("file://{}", project_clj.display()), "type": 2 }] }),
    );

    let deadline = Instant::now() + TIMEOUT;
    loop {
        let result = client.goto_definition(&consumer, line, ch);
        if let Some(uri) = result["uri"].as_str() {
            assert!(
                uri.starts_with("jar:file://") && uri.ends_with("!/mylib/util.clj"),
                "expected jar navigation after project.clj edit, got {}",
                uri
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "new dep not indexed after project.clj change"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn test_e2e_completion_from_directory_library() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let libdir = tempfile::TempDir::new().unwrap();
    let lib_src = libdir.path().join("src");
    std::fs::create_dir_all(lib_src.join("gitlib")).unwrap();
    std::fs::write(
        lib_src.join("gitlib/util.clj"),
        "(ns gitlib.util)\n\n(defn helper\n  \"From a git dep.\"\n  [x]\n  x)\n",
    )
    .unwrap();

    let cpcache = root.join(".cpcache");
    std::fs::create_dir_all(&cpcache).unwrap();
    std::fs::write(cpcache.join("1.cp"), lib_src.display().to_string()).unwrap();

    let consumer = root.join("src/uses_gitlib.clj");
    std::fs::write(
        &consumer,
        "(ns uses-gitlib\n  (:require [gitlib.util :as u]))\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");
    client.did_open(&consumer);

    client.did_change_insert(&consumer, 2, 0, "(u/hel");
    let result = client.completion_items(&consumer, 2, 6);

    let labels: Vec<&str> = result
        .as_array()
        .expect("expected CompletionItem array")
        .iter()
        .filter_map(|i| i["label"].as_str())
        .collect();
    assert!(
        labels.contains(&"u/helper"),
        "expected u/helper completion from directory lib, got {:?}",
        labels
    );
}

#[test]
fn test_e2e_completion_namespaces_and_aliases() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let libdir = tempfile::TempDir::new().unwrap();
    let lib_src = libdir.path().join("src");
    std::fs::create_dir_all(lib_src.join("gitlib")).unwrap();
    std::fs::write(
        lib_src.join("gitlib/util.clj"),
        "(ns gitlib.util)\n\n(defn helper [x] x)\n",
    )
    .unwrap();

    let cpcache = root.join(".cpcache");
    std::fs::create_dir_all(&cpcache).unwrap();
    std::fs::write(cpcache.join("1.cp"), lib_src.display().to_string()).unwrap();

    let consumer = root.join("src/uses_gitlib.clj");
    std::fs::write(
        &consumer,
        "(ns uses-gitlib\n  (:require [gitlib.util :as u]))\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");
    client.did_open(&consumer);

    // Namespace completion, as when typing inside (:require [gitli…])
    client.did_change_insert(&consumer, 2, 0, "gitli\n");
    let result = client.completion_items(&consumer, 2, 5);
    let labels: Vec<&str> = result
        .as_array()
        .expect("expected CompletionItem array")
        .iter()
        .filter_map(|i| i["label"].as_str())
        .collect();
    assert!(
        labels.contains(&"gitlib.util"),
        "expected gitlib.util namespace completion, got {:?}",
        labels
    );

    // Alias completion: typing "u" offers the alias itself
    client.did_change_insert(&consumer, 3, 0, "(u");
    let result = client.completion_items(&consumer, 3, 2);
    let items = result.as_array().expect("expected CompletionItem array");
    let alias = items
        .iter()
        .find(|i| i["label"] == "u" && i["detail"] == "alias for gitlib.util");
    assert!(
        alias.is_some(),
        "expected alias completion for u, got {:?}",
        items
            .iter()
            .map(|i| i["label"].as_str().unwrap_or(""))
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_e2e_signature_help_while_typing_call() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let utils = root.join("src/utils.clj");
    client.did_open(&utils);

    let last_line = std::fs::read_to_string(&utils).unwrap().lines().count() as u32;

    // Project fn via alias: "(core/add " — first parameter active
    client.did_change_insert(&utils, last_line, 0, "(core/add \n");
    let result = client.signature_help(&utils, last_line, 10);
    assert!(!result.is_null(), "no signature help for core/add");
    let label = result["signatures"][0]["label"].as_str().unwrap();
    assert_eq!(label, "(add a b)");
    assert_eq!(result["activeParameter"], json!(0));
    assert_eq!(
        result["signatures"][0]["parameters"][0]["label"],
        json!("a")
    );

    // Second argument: "(core/add 1 " — second parameter active
    client.did_change_insert(&utils, last_line + 1, 0, "(core/add 1 \n");
    let result = client.signature_help(&utils, last_line + 1, 12);
    assert_eq!(result["activeParameter"], json!(1));

    // clojure.core builtin with multiple arities
    client.did_change_insert(&utils, last_line + 2, 0, "(reduce f init ");
    let result = client.signature_help(&utils, last_line + 2, 15);
    assert!(!result.is_null(), "no signature help for reduce");
    let labels: Vec<&str> = result["signatures"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["label"].as_str())
        .collect();
    assert!(
        labels.iter().any(|l| l.contains("coll")),
        "expected reduce arities, got {:?}",
        labels
    );
}

#[test]
fn test_e2e_definition_on_require_alias_and_namespace() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let utils = root.join("src/utils.clj");
    client.did_open(&utils);

    // Cursor on the alias in `[simple.core :as core]`
    let (line, ch) = position_of(&utils, "core]");
    let result = client.goto_definition(&utils, line, ch);
    assert!(!result.is_null(), "no definition for require alias");
    let uri = result["uri"].as_str().expect("expected Location");
    assert!(
        uri.ends_with("/src/core.clj"),
        "alias should navigate to core.clj, got {}",
        uri
    );
    assert_eq!(result["range"]["start"]["line"], json!(0));

    // Cursor on the namespace symbol itself
    let (line, ch) = position_of(&utils, "simple.core");
    let result = client.goto_definition(&utils, line, ch);
    assert!(!result.is_null(), "no definition for required namespace");
    let uri = result["uri"].as_str().expect("expected Location");
    assert!(
        uri.ends_with("/src/core.clj"),
        "namespace should navigate to core.clj, got {}",
        uri
    );
}

#[test]
fn test_e2e_definition_on_core_builtin_navigates_into_clojure_jar() {
    // `defn`, `or`, `cond`… are ordinary definitions inside the clojure JAR;
    // bare usages must navigate into it even though the static core list
    // answers hover/completion.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let jar_path = root.join("clojure-x.jar");
    let jar_file = std::fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(jar_file);
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("clojure/core.clj", opts).unwrap();
    zip.write_all(
        b"(ns ^{:doc \"core\"} clojure.core)\n\n(defmacro or\n  \"Evaluates exprs one at a time.\"\n  ([] nil)\n  ([x] x)\n  ([x & next] nil))\n",
    )
    .unwrap();
    zip.finish().unwrap();

    let cpcache = root.join(".cpcache");
    std::fs::create_dir_all(&cpcache).unwrap();
    std::fs::write(cpcache.join("1.cp"), jar_path.display().to_string()).unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");

    let utils = root.join("src/utils.clj");
    client.did_open(&utils);

    let last_line = std::fs::read_to_string(&utils).unwrap().lines().count() as u32;
    client.did_change_insert(&utils, last_line, 0, "(or 1 2)\n");
    let result = client.goto_definition(&utils, last_line, 2);

    assert!(
        !result.is_null(),
        "goto-definition on core builtin returned null"
    );
    let uri = result["uri"].as_str().expect("expected Location");
    assert!(
        uri.starts_with("jar:file://") && uri.ends_with("!/clojure/core.clj"),
        "expected jar URI into clojure core, got {}",
        uri
    );
    // name_range points at `or` on the defmacro line
    assert_eq!(result["range"]["start"]["line"], json!(2));
}

#[test]
fn test_e2e_definition_on_alias_shadowing_core_symbol() {
    // `[simple.core :as str]`: the alias shadows clojure.core/str. On the
    // alias declaration it must navigate to the namespace; on a body usage
    // of bare `str` (which is clojure.core/str) it must not.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let consumer = root.join("src/shadow.clj");
    std::fs::write(
        &consumer,
        "(ns shadow\n  (:require [simple.core :as str]))\n\n(defn f [x]\n  (str/add x 1)\n  (str \"x\"))\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.did_open(&consumer);

    // Alias declaration → namespace file
    let (line, ch) = position_of(&consumer, ":as str");
    let result = client.goto_definition(&consumer, line, ch + 2);
    assert!(
        !result.is_null(),
        "alias shadowing a core symbol did not navigate"
    );
    let uri = result["uri"].as_str().expect("expected Location");
    assert!(
        uri.ends_with("/src/core.clj"),
        "expected core.clj, got {}",
        uri
    );

    // Bare core-symbol usage in the body → no navigation (clojure.core/str)
    let (line, ch) = position_of(&consumer, "(str \"x\")");
    let result = client.goto_definition(&consumer, line, ch);
    assert!(
        result.is_null(),
        "bare core symbol in body must not navigate to the alias ns: {:?}",
        result
    );
}

#[test]
fn test_e2e_document_symbols_outline() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let core = root.join("src/core.clj");
    client.did_open(&core);

    let result = client.document_symbols(&core);
    assert!(!result.is_null(), "documentSymbol returned null");
    let symbols = result.as_array().expect("expected DocumentSymbol array");

    let names: Vec<&str> = symbols.iter().filter_map(|s| s["name"].as_str()).collect();
    assert_eq!(names, vec!["VERSION", "add", "multiply"]);

    // SymbolKind: Variable = 13, Function = 12
    let version = &symbols[0];
    assert_eq!(version["kind"], json!(13));
    let add = &symbols[1];
    assert_eq!(add["kind"], json!(12));

    // selectionRange points at the name, range covers the whole form
    let (def_line, _) = position_of(&core, "defn add");
    assert_eq!(add["selectionRange"]["start"]["line"], json!(def_line));
    assert_eq!(add["range"]["start"]["line"], json!(def_line));
    assert!(add["range"]["end"]["line"].as_u64().unwrap() > def_line as u64);

    // Live (unsaved) edits are reflected in the outline
    let last_line = std::fs::read_to_string(&core).unwrap().lines().count() as u32;
    client.did_change_insert(&core, last_line, 0, "(defn fresh [] 1)\n");
    let result = client.document_symbols(&core);
    let names: Vec<String> = result
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(String::from))
        .collect();
    assert!(
        names.contains(&"fresh".to_string()),
        "outline missing unsaved defn: {:?}",
        names
    );

    // Non-file documents (jar: virtual sources opened by the editor) must
    // be outlined from their open text, not rejected with a server error.
    let jar_uri = "jar:file:///some/lib.jar!/mylib/util.clj";
    client.notify(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": jar_uri,
                "languageId": "clojure",
                "version": 1,
                "text": "(ns mylib.util)\n\n(defn helper [x] x)\n"
            }
        }),
    );
    let result = client.request(
        "textDocument/documentSymbol",
        json!({ "textDocument": { "uri": jar_uri } }),
    );
    let names: Vec<&str> = result
        .as_array()
        .expect("expected symbols for jar document")
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert_eq!(names, vec!["helper"]);
}

/// Indent-on-type round trip: with the buffer in its post-Enter state (the
/// newline already inserted), `onTypeFormatting` must return an edit that
/// indents the new line to the structural column.
#[test]
fn test_e2e_on_type_formatting_indents_new_line() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    let init = client.initialize(&root);
    assert!(
        init["capabilities"]["documentOnTypeFormattingProvider"].is_object(),
        "documentOnTypeFormattingProvider not advertised: {}",
        init["capabilities"]
    );
    assert_eq!(
        init["capabilities"]["documentOnTypeFormattingProvider"]["firstTriggerCharacter"],
        json!("\n")
    );

    // Enter was pressed inside `(let [a 1|])`: the closer now starts line 3.
    let source = "(ns app.indent)\n\n(let [a 1\n])\n";
    let path = root.join("src/indent_fixture.clj");
    std::fs::write(&path, source).unwrap();
    client.did_open(&path);

    let result = client.on_type_formatting(&path, 3, 0, "\n");
    let edits = result.as_array().expect("expected TextEdit array");
    assert_eq!(
        apply_edits(source, edits),
        "(ns app.indent)\n\n(let [a 1\n      ])\n",
        "new line should align under `a`"
    );

    // Inside a multiline string no edit is offered — indentation would
    // change the string's value.
    let str_source = "(def s \"line\n\")\n";
    let str_path = root.join("src/indent_string_fixture.clj");
    std::fs::write(&str_path, str_source).unwrap();
    client.did_open(&str_path);
    let result = client.on_type_formatting(&str_path, 1, 0, "\n");
    assert!(result.is_null(), "expected no edits in string: {}", result);
}

#[test]
fn test_e2e_ignored_forms() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    let init = client.initialize(&root);

    // The Tier-1 semantic-tokens capability was retired — dimming is served
    // over the custom `clojurePulse/ignoredForms` request instead.
    assert!(
        init["capabilities"]["semanticTokensProvider"].is_null(),
        "semanticTokensProvider should no longer be advertised: {}",
        init["capabilities"]
    );

    // A `;` line comment (line 0), a plain def (line 3), a `#_` discard
    // (line 6), and a multi-line `(comment …)` block (lines 7-9).
    let src = "; a line comment\n(ns tokens.demo)\n\n(def n 42)\n(def s \"hello\")\n(def k :some/key)\n#_(unused 1)\n(comment\n  (+ 1 2)\n  :done)\n";
    let file = root.join("src/tokens.clj");
    std::fs::write(&file, src).unwrap();
    client.did_open(&file);

    let result = client.ignored_forms(&file);
    let ranges = result
        .as_array()
        .expect("ignoredForms result should be an array");

    let covers = |r: &Value, l: u64| {
        r["start"]["line"].as_u64().unwrap() <= l && l <= r["end"]["line"].as_u64().unwrap()
    };

    // Exactly the two ignored forms: the `#_` discard and the `(comment …)`.
    assert_eq!(ranges.len(), 2, "expected 2 ignored-form ranges: {result}");
    assert!(
        ranges.iter().any(|r| r["start"]["line"] == json!(6)),
        "a range should start on the #_ line (6): {result}"
    );
    assert!(
        ranges.iter().any(|r| r["start"]["line"] == json!(7)),
        "a range should start on the (comment …) line (7): {result}"
    );

    // The `;` line comment (0) and the plain `(def n 42)` (3) are never dimmed.
    assert!(
        !ranges.iter().any(|r| covers(r, 0) || covers(r, 3)),
        "line comments and plain code must not be dimmed: {result}"
    );
}

#[test]
fn test_e2e_workspace_symbols_ranked_and_project_only() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    // A library JAR defining `addition` — must NOT appear in results
    let jar_path = root.join("mylib.jar");
    let jar_file = std::fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(jar_file);
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("mylib/util.clj", opts).unwrap();
    zip.write_all(b"(ns mylib.util)\n\n(defn addition [x] x)\n")
        .unwrap();
    zip.finish().unwrap();
    let cpcache = root.join(".cpcache");
    std::fs::create_dir_all(&cpcache).unwrap();
    std::fs::write(cpcache.join("1.cp"), jar_path.display().to_string()).unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");

    let result = client.workspace_symbols("add");
    let symbols = result.as_array().expect("expected SymbolInformation array");
    let names: Vec<&str> = symbols.iter().filter_map(|s| s["name"].as_str()).collect();

    // Exact match ranks before prefix match; library symbol excluded
    assert_eq!(names, vec!["add", "add-and-double"]);
    let add = &symbols[0];
    assert_eq!(add["containerName"], json!("simple.core"));
    assert!(add["location"]["uri"]
        .as_str()
        .unwrap()
        .ends_with("/src/core.clj"));

    // Subsequence matching: "aad" finds add-and-double
    let result = client.workspace_symbols("aad");
    let names: Vec<&str> = result
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        names.contains(&"add-and-double"),
        "subsequence match failed: {:?}",
        names
    );
}

#[test]
fn test_e2e_add_missing_require() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    // consumer.clj uses `helpers/greet` without requiring simple.helpers.
    let consumer = root.join("src/consumer.clj");
    client.did_open(&consumer);

    let (line, ch) = position_of(&consumer, "helpers/greet");
    let result = client.code_action(&consumer, line, ch);

    let actions = result.as_array().expect("expected code action array");
    let action = actions
        .iter()
        .find(|a| {
            a["title"]
                .as_str()
                .map(|t| t.contains("[simple.helpers :as helpers]"))
                .unwrap_or(false)
        })
        .expect("expected add-require action for simple.helpers");

    assert_eq!(action["kind"], json!("quickfix"));

    // The edit inserts the require spec into consumer.clj.
    let edits = action["edit"]["changes"]
        .as_object()
        .expect("expected WorkspaceEdit.changes")
        .values()
        .next()
        .expect("expected edits for the file")
        .as_array()
        .unwrap();
    let new_text = edits[0]["newText"].as_str().unwrap();
    assert!(
        new_text.contains("(:require [simple.helpers :as helpers])"),
        "unexpected edit text: {}",
        new_text
    );
}

#[test]
fn test_e2e_unresolved_namespace_diagnostic() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    // consumer.clj uses `helpers/greet` without requiring simple.helpers.
    let consumer = root.join("src/consumer.clj");
    client.did_open(&consumer);

    let params = client.wait_for_diagnostics("/src/consumer.clj");
    let diags = params["diagnostics"].as_array().expect("diagnostics array");
    let unresolved = diags
        .iter()
        .find(|d| d["code"] == json!("unresolved-namespace"))
        .expect("expected an unresolved-namespace diagnostic");
    assert_eq!(unresolved["severity"], json!(2)); // WARNING
    assert!(unresolved["message"].as_str().unwrap().contains("helpers"));

    // VS Code requests code actions for the squiggle, passing the diagnostic.
    // The add-require fix is returned and carries that diagnostic so the
    // client binds them.
    let result = client.code_action_for_diagnostic(&consumer, unresolved);
    let actions = result.as_array().expect("code action array");
    let action = actions
        .iter()
        .find(|a| {
            a["title"]
                .as_str()
                .map(|t| t.contains("[simple.helpers :as helpers]"))
                .unwrap_or(false)
        })
        .expect("expected add-require action");
    assert_eq!(action["kind"], json!("quickfix"));
    assert_eq!(
        action["diagnostics"][0]["code"],
        json!("unresolved-namespace"),
        "fix should carry the diagnostic it resolves"
    );
}

#[test]
fn test_e2e_clean_ns_removes_unused_require() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    // scratch.clj requires clojure.string (unused) and simple.helpers (used).
    let scratch = root.join("src/scratch.clj");
    let source = "(ns simple.scratch\n  (:require [clojure.string :as str]\n            \
                  [simple.helpers :as helpers]))\n\n(defn run []\n  (helpers/greet \"hi\"))\n";
    std::fs::write(&scratch, source).unwrap();
    client.did_open(&scratch);

    // VS Code's "Organize Imports" path: a code-action request restricted to
    // the source.organizeImports kind.
    let result = client.code_action_only(&scratch, &["source.organizeImports"]);
    let actions = result.as_array().expect("expected code action array");
    let action = actions
        .iter()
        .find(|a| a["kind"] == json!("source.organizeImports"))
        .expect("expected a clean-namespace source action");
    assert!(
        action["title"]
            .as_str()
            .map(|t| t.contains("Clean"))
            .unwrap_or(false),
        "unexpected title: {}",
        action["title"]
    );

    let edits = action["edit"]["changes"]
        .as_object()
        .expect("expected WorkspaceEdit.changes")
        .values()
        .next()
        .expect("expected edits for the file")
        .as_array()
        .unwrap();
    let cleaned = apply_edits(source, edits);
    assert!(
        !cleaned.contains("clojure.string"),
        "unused require not removed:\n{}",
        cleaned
    );
    assert!(
        cleaned.contains("[simple.helpers :as helpers]"),
        "used require dropped:\n{}",
        cleaned
    );
    assert!(
        cleaned.contains("(helpers/greet \"hi\")"),
        "body changed:\n{}",
        cleaned
    );
}

#[test]
fn test_e2e_unused_namespace_diagnostic() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    // scratch.clj requires clojure.string (unused) and simple.helpers (used).
    let scratch = root.join("src/scratch.clj");
    let source = "(ns simple.scratch\n  (:require [clojure.string :as str]\n            \
                  [simple.helpers :as helpers]))\n\n(defn run []\n  (helpers/greet \"hi\"))\n";
    std::fs::write(&scratch, source).unwrap();
    client.did_open(&scratch);

    let params = client.wait_for_diagnostics("/src/scratch.clj");
    let diags = params["diagnostics"].as_array().expect("diagnostics array");

    let unused: Vec<&Value> = diags
        .iter()
        .filter(|d| d["code"] == json!("unused-namespace"))
        .collect();
    assert_eq!(
        unused.len(),
        1,
        "expected exactly one unused-namespace diagnostic, got {}",
        params["diagnostics"]
    );
    let d = unused[0];
    assert_eq!(d["severity"], json!(2)); // WARNING
    assert_eq!(d["source"], json!("clj-pulse"));
    assert!(
        d["message"].as_str().unwrap().contains("clojure.string"),
        "message: {}",
        d["message"]
    );
    // DiagnosticTag::UNNECESSARY (1) so editors fade the unused require.
    assert_eq!(d["tags"], json!([1]));
    // The used require is never flagged.
    assert!(
        unused.iter().all(|d| !d["message"]
            .as_str()
            .unwrap_or("")
            .contains("simple.helpers")),
        "used require flagged as unused: {}",
        params["diagnostics"]
    );
}

#[test]
fn test_e2e_duplicate_require_diagnostic() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    // dup.clj requires clojure.string twice with different aliases (both used,
    // so the only finding is the duplicate, not an unused require).
    let dup = root.join("src/dup.clj");
    let source = "(ns simple.dup\n  (:require [clojure.string :as str]\n            \
                  [clojure.string :as s]))\n\n(defn run []\n  [(str/trim \"a\") (s/upper-case \"b\")])\n";
    std::fs::write(&dup, source).unwrap();
    client.did_open(&dup);

    let params = client.wait_for_diagnostics("/src/dup.clj");
    let diags = params["diagnostics"].as_array().expect("diagnostics array");

    let duplicates: Vec<&Value> = diags
        .iter()
        .filter(|d| d["code"] == json!("duplicate-require"))
        .collect();
    assert_eq!(
        duplicates.len(),
        1,
        "expected exactly one duplicate-require diagnostic, got {}",
        params["diagnostics"]
    );
    let d = duplicates[0];
    assert_eq!(d["severity"], json!(2)); // WARNING
    assert_eq!(d["source"], json!("clj-pulse"));
    assert!(
        d["message"].as_str().unwrap().contains("clojure.string"),
        "message: {}",
        d["message"]
    );
    // duplicate-require is intentionally not tagged UNNECESSARY (the binding is
    // used, so it is redundant rather than dead) — the tag must be absent.
    assert_eq!(d["tags"], json!(null));
    // It points at the second occurrence (line 2, 0-based).
    assert_eq!(d["range"]["start"]["line"], json!(2));
}

#[test]
fn test_e2e_find_references() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let core = root.join("src/core.clj");
    let utils = root.join("src/utils.clj");
    client.did_open(&core);

    // From the definition site, with declaration included
    let (line, ch) = position_of(&core, "add"); // `(defn add` is the first match
    let result = client.references(&core, line, ch, true);
    assert!(!result.is_null(), "references returned null");
    let locs = result.as_array().unwrap();
    let uris: Vec<&str> = locs.iter().filter_map(|l| l["uri"].as_str()).collect();
    assert_eq!(
        locs.len(),
        2,
        "expected declaration + usage, got {:?}",
        locs
    );
    assert!(uris.iter().any(|u| u.ends_with("/src/core.clj")));
    assert!(uris.iter().any(|u| u.ends_with("/src/utils.clj")));

    // Usage range covers only `add`, not the `core/` alias
    let usage = locs
        .iter()
        .find(|l| l["uri"].as_str().unwrap().ends_with("/src/utils.clj"))
        .unwrap();
    let utils_text = std::fs::read_to_string(&utils).unwrap();
    let usage_line = utils_text.lines().nth(6).unwrap(); // "  (* 2 (core/add x y)))"
    let name_col = usage_line.find("core/add").unwrap() + "core/".len();
    assert_eq!(usage["range"]["start"]["character"], json!(name_col));

    // Without declaration: only the usage
    let result = client.references(&core, line, ch, false);
    let locs = result.as_array().unwrap();
    assert_eq!(locs.len(), 1);
    assert!(locs[0]["uri"].as_str().unwrap().ends_with("/src/utils.clj"));

    // From the usage site, resolution gives the same answer
    let (uline, uch) = position_of(&utils, "core/add");
    client.did_open(&utils);
    let result = client.references(&utils, uline, uch, true);
    assert_eq!(result.as_array().unwrap().len(), 2);
}

#[test]
fn test_e2e_references_find_usage_in_unopened_alias_test_dir() {
    // A usage in test/ declared via an alias :extra-paths must be found at
    // startup, without opening the test file.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();
    std::fs::write(
        root.join("deps.edn"),
        "{:paths [\"src\"]\n :aliases {:test {:extra-paths [\"test\"]}}\n \
         :deps {org.clojure/clojure {:mvn/version \"1.11.1\"}}}\n",
    )
    .unwrap();
    let test_file = root.join("test/core_test.clj");
    std::fs::create_dir_all(test_file.parent().unwrap()).unwrap();
    std::fs::write(
        &test_file,
        "(ns simple.core-test\n  (:require [simple.core :as core]))\n(core/add 1 2)\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("Indexed");

    let core = root.join("src/core.clj");
    client.did_open(&core); // open ONLY the definition file
    let (line, ch) = position_of(&core, "add");
    let result = client.references(&core, line, ch, false);
    let locs = result.as_array().cloned().unwrap_or_default();
    assert!(
        locs.iter().any(|l| l["uri"]
            .as_str()
            .map(|u| u.ends_with("/test/core_test.clj"))
            .unwrap_or(false)),
        "test/ usage (alias :extra-paths) not found without opening it: {:?}",
        locs
    );
}

#[test]
fn test_e2e_references_find_usage_in_unopened_default_test_dir() {
    // Even when test/ is declared nowhere, the default src/test scan roots
    // index it at startup so its usages are found without opening the file.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();
    // deps.edn keeps the default :paths ["src"] (no :test alias).
    let test_file = root.join("test/core_test.clj");
    std::fs::create_dir_all(test_file.parent().unwrap()).unwrap();
    std::fs::write(
        &test_file,
        "(ns simple.core-test\n  (:require [simple.core :as core]))\n(core/add 1 2)\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("Indexed");

    let core = root.join("src/core.clj");
    client.did_open(&core);
    let (line, ch) = position_of(&core, "add");
    let result = client.references(&core, line, ch, false);
    let locs = result.as_array().cloned().unwrap_or_default();
    assert!(
        locs.iter().any(|l| l["uri"]
            .as_str()
            .map(|u| u.ends_with("/test/core_test.clj"))
            .unwrap_or(false)),
        "test/ usage (default scan root) not found without opening it: {:?}",
        locs
    );
}

#[test]
fn test_e2e_references_work_without_indexed_definition() {
    // References of an alias-qualified usage must work even when the target
    // library isn't indexed (yet) — the fqn is derivable from the alias.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let consumer = root.join("src/uses_unknown.clj");
    std::fs::write(
        &consumer,
        "(ns uses-unknown\n  (:require [unknown.lib :as ul]))\n\n(ul/go 1)\n(ul/go 2)\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.did_open(&consumer);

    let (line, ch) = position_of(&consumer, "ul/go");
    let result = client.references(&consumer, line, ch, true);
    assert!(!result.is_null(), "references returned null");
    assert_eq!(
        result.as_array().unwrap().len(),
        2,
        "both usages must be found: {:?}",
        result
    );
}

#[test]
fn test_e2e_rename_local_never_touches_shadowed_global() {
    // `(defn f2 [add] add)` — the param shadows simple.core/add. Renaming it
    // edits the param and its body usage only; the global var (its defn earlier
    // in core.clj, its use in utils.clj) is never touched. References resolve
    // the same way.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let core = root.join("src/core.clj");
    client.did_open(&core);
    let last_line = std::fs::read_to_string(&core).unwrap().lines().count() as u32;
    client.did_change_insert(&core, last_line, 0, "(defn f2 [add] add)\n");

    // Cursor on the param `add` (col 11)
    let result = client.rename(&core, last_line, 11, "plus");
    let changes = result["changes"]
        .as_object()
        .unwrap_or_else(|| panic!("expected a WorkspaceEdit, got: {}", result));
    assert_eq!(
        changes.len(),
        1,
        "only the shadowing file is edited: {:?}",
        changes.keys().collect::<Vec<_>>()
    );
    let (uri, edits) = changes.iter().next().unwrap();
    assert!(uri.ends_with("/src/core.clj"), "got {}", uri);
    let edits = edits.as_array().unwrap();
    assert_eq!(edits.len(), 2, "param + body usage only: {:?}", edits);
    let mut cols: Vec<u64> = edits
        .iter()
        .map(|e| {
            assert_eq!(e["newText"], json!("plus"));
            assert_eq!(
                e["range"]["start"]["line"],
                json!(last_line),
                "edit off the shadowing form: {:?}",
                e
            );
            e["range"]["start"]["character"].as_u64().unwrap()
        })
        .collect();
    cols.sort_unstable();
    assert_eq!(cols, vec![10, 15], "binding + usage columns: {:?}", edits);

    // References resolve to the local (param binding + body usage, both on the
    // inserted `f2` line) and must NOT reach the global `simple.core/add` — not
    // its defn earlier in core.clj, not its usage in utils.clj.
    let refs = client.references(&core, last_line, 11, true);
    let locs = refs
        .as_array()
        .unwrap_or_else(|| panic!("expected local references, got: {}", refs));
    assert_eq!(locs.len(), 2, "param binding + body usage only: {:?}", locs);
    assert!(
        locs.iter().all(|l| {
            l["uri"].as_str().unwrap().ends_with("/src/core.clj")
                && l["range"]["start"]["line"] == json!(last_line)
        }),
        "local refs stay on the shadowing form, never the global: {:?}",
        locs
    );
}

#[test]
fn test_e2e_rename_local_in_let() {
    // Renaming a `let` binding rewrites the binding site and every in-scope
    // usage, all within the one file.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let locals = root.join("src/locals.clj");
    client.did_open(&locals);

    let (line, ch) = position_of(&locals, "base");
    let result = client.rename(&locals, line, ch, "b0");
    let changes = result["changes"]
        .as_object()
        .unwrap_or_else(|| panic!("expected a WorkspaceEdit, got: {}", result));
    assert_eq!(
        changes.len(),
        1,
        "one file: {:?}",
        changes.keys().collect::<Vec<_>>()
    );
    let (uri, edits) = changes.iter().next().unwrap();
    assert!(uri.ends_with("/src/locals.clj"), "got {}", uri);
    let edits = edits.as_array().unwrap();
    assert_eq!(edits.len(), 3, "binding + two usages: {:?}", edits);
    assert!(
        edits.iter().all(|e| e["newText"] == json!("b0")),
        "{:?}",
        edits
    );
    let mut lines: Vec<u64> = edits
        .iter()
        .map(|e| e["range"]["start"]["line"].as_u64().unwrap())
        .collect();
    lines.sort_unstable();
    assert_eq!(
        lines,
        vec![3, 4, 5],
        "binding, RHS use, body use: {:?}",
        edits
    );
}

#[test]
fn test_e2e_rename_local_rejects_capture_by_existing_binding() {
    // Renaming `a` to `b` inside `(let [a 1 b 2] …)` would capture the usage
    // under the wrong binding, so it is refused rather than silently applied.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let core = root.join("src/core.clj");
    client.did_open(&core);
    let last_line = std::fs::read_to_string(&core).unwrap().lines().count() as u32;
    client.did_change_insert(
        &core,
        last_line,
        0,
        "(defn f4 [] (let [a 1 b 2] (+ a b)))\n",
    );

    // Cursor on the binding `a` (col 18).
    let error = client.request_expect_error(
        "textDocument/rename",
        json!({
            "textDocument": { "uri": format!("file://{}", core.display()) },
            "position": { "line": last_line, "character": 18 },
            "newName": "b"
        }),
    );
    assert!(
        error["message"].as_str().unwrap().contains("already bound"),
        "got: {}",
        error
    );

    // A free name still renames fine from the same position.
    let result = client.rename(&core, last_line, 18, "a2");
    let changes = result["changes"]
        .as_object()
        .expect("rename should succeed");
    assert_eq!(
        changes.values().next().unwrap().as_array().unwrap().len(),
        2
    );
}

#[test]
fn test_e2e_rename_rejects_keys_destructured_local() {
    // `{:keys [k]}` makes the binding name double as the map key, so renaming
    // it would silently change what is looked up. Rejected with a hint.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let core = root.join("src/core.clj");
    client.did_open(&core);
    let last_line = std::fs::read_to_string(&core).unwrap().lines().count() as u32;
    client.did_change_insert(&core, last_line, 0, "(defn f3 [{:keys [k]}] (inc k))\n");

    // Cursor on the `k` inside `(inc k)`.
    let error = client.request_expect_error(
        "textDocument/rename",
        json!({
            "textDocument": { "uri": format!("file://{}", core.display()) },
            "position": { "line": last_line, "character": 28 },
            "newName": "kk"
        }),
    );
    assert!(
        error["message"].as_str().unwrap().contains("destructured"),
        "got: {}",
        error
    );
}

#[test]
fn test_e2e_classpath_change_drops_stale_libs() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let make_jar = |name: &str, entry: &str, content: &[u8]| {
        let jar_path = root.join(name);
        let jar_file = std::fs::File::create(&jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(jar_file);
        let opts = zip::write::SimpleFileOptions::default();
        zip.start_file(entry, opts).unwrap();
        zip.write_all(content).unwrap();
        zip.finish().unwrap();
        jar_path
    };
    let jar_a = make_jar(
        "liba.jar",
        "mylib/util.clj",
        b"(ns mylib.util)\n(defn helper [x] x)\n",
    );
    let jar_b = make_jar(
        "libb.jar",
        "otherlib/core.clj",
        b"(ns otherlib.core)\n(defn other-fn [x] x)\n",
    );

    let cpcache = root.join(".cpcache");
    std::fs::create_dir_all(&cpcache).unwrap();
    let cp_file = cpcache.join("1.cp");
    std::fs::write(&cp_file, jar_a.display().to_string()).unwrap();

    let consumer = root.join("src/uses_lib.clj");
    std::fs::write(
        &consumer,
        "(ns uses-lib\n  (:require [mylib.util :as u]))\n\n(u/helper 42)\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");
    client.did_open(&consumer);

    let (line, ch) = position_of(&consumer, "u/helper");
    assert!(
        !client.goto_definition(&consumer, line, ch).is_null(),
        "lib A must resolve before the classpath change"
    );

    // Dependency swapped: A removed, B added
    std::fs::write(&cp_file, jar_b.display().to_string()).unwrap();
    client.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": format!("file://{}", cp_file.display()), "type": 2 }] }),
    );

    let deadline = Instant::now() + TIMEOUT;
    loop {
        let result = client.goto_definition(&consumer, line, ch);
        if result.is_null() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "stale lib A symbol still resolves after classpath change"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn test_e2e_deps_edn_change_reindexes_project_paths() {
    // Adding a source root to :paths (e.g. via git pull) must index its
    // files without a restart.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    std::fs::create_dir_all(root.join("extra")).unwrap();
    std::fs::write(
        root.join("extra/more.clj"),
        "(ns extra.more)\n\n(defn extra-fn [x] x)\n",
    )
    .unwrap();
    let consumer = root.join("src/uses_extra.clj");
    std::fs::write(
        &consumer,
        "(ns uses-extra\n  (:require [extra.more :as em]))\n\n(em/extra-fn 1)\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.did_open(&consumer);
    let (line, ch) = position_of(&consumer, "em/extra-fn");
    assert!(
        client.goto_definition(&consumer, line, ch).is_null(),
        "extra/ must not be indexed while outside :paths"
    );

    // :paths gains the new root
    let deps = root.join("deps.edn");
    std::fs::write(
        &deps,
        "{:paths [\"src\" \"extra\"]\n :deps {org.clojure/clojure {:mvn/version \"1.11.1\"}}}\n",
    )
    .unwrap();
    client.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": format!("file://{}", deps.display()), "type": 2 }] }),
    );

    let deadline = Instant::now() + TIMEOUT;
    let mut result = Value::Null;
    while Instant::now() < deadline {
        result = client.goto_definition(&consumer, line, ch);
        if !result.is_null() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let uri = result["uri"]
        .as_str()
        .expect("definition after :paths change");
    assert!(uri.ends_with("/extra/more.clj"), "got {}", uri);
}

#[test]
fn test_e2e_rename_across_files() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    // Third file referring to `add` via :refer — rename must fix the
    // refer vector and the bare usage too
    std::fs::write(
        root.join("src/refers.clj"),
        "(ns simple.refers\n  (:require [simple.core :refer [add]]))\n\n(add 1 2)\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let core = root.join("src/core.clj");
    client.did_open(&core);

    let (line, ch) = position_of(&core, "add");
    let result = client.rename(&core, line, ch, "plus");
    assert!(!result.is_null(), "rename returned null");
    let changes = result["changes"]
        .as_object()
        .expect("expected WorkspaceEdit.changes");

    assert_eq!(
        changes.len(),
        3,
        "expected 3 files edited: {:?}",
        changes.keys().collect::<Vec<_>>()
    );

    let edits_for = |suffix: &str| -> Vec<Value> {
        changes
            .iter()
            .find(|(uri, _)| uri.ends_with(suffix))
            .unwrap_or_else(|| panic!("no edits for {}", suffix))
            .1
            .as_array()
            .unwrap()
            .clone()
    };

    // Declaration edit
    let core_edits = edits_for("/src/core.clj");
    assert_eq!(core_edits.len(), 1);
    assert_eq!(core_edits[0]["newText"], json!("plus"));
    assert_eq!(core_edits[0]["range"]["start"]["line"], json!(line));

    // Alias-qualified usage: edit covers only the name part
    let utils_edits = edits_for("/src/utils.clj");
    assert_eq!(utils_edits.len(), 1);
    let utils_text = std::fs::read_to_string(root.join("src/utils.clj")).unwrap();
    let usage_line = utils_text.lines().nth(6).unwrap();
    let name_col = usage_line.find("core/add").unwrap() + "core/".len();
    assert_eq!(
        utils_edits[0]["range"]["start"]["character"],
        json!(name_col)
    );
    assert_eq!(
        utils_edits[0]["range"]["end"]["character"],
        json!(name_col + 3)
    );

    // :refer vector entry + bare usage
    let refers_edits = edits_for("/src/refers.clj");
    assert_eq!(
        refers_edits.len(),
        2,
        "refer vector + usage: {:?}",
        refers_edits
    );
}

#[test]
fn test_e2e_rename_rejects_library_symbols() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let utils = root.join("src/utils.clj");
    client.did_open(&utils);

    // `str` in the body is clojure.core/str
    let (line, ch) = position_of(&utils, "(str \"Hello");
    let error = client.request_expect_error(
        "textDocument/rename",
        json!({
            "textDocument": { "uri": format!("file://{}", utils.display()) },
            "position": { "line": line, "character": ch + 1 },
            "newName": "my-str"
        }),
    );
    let msg = error["message"].as_str().unwrap();
    assert!(
        msg.contains("rename"),
        "expected a rename rejection message, got: {}",
        msg
    );
}

#[test]
fn test_e2e_rename_uses_unsaved_edits() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let core = root.join("src/core.clj");
    let utils = root.join("src/utils.clj");
    client.did_open(&core);
    client.did_open(&utils);

    // Add an unsaved usage of core/add in utils.clj
    let last_line = std::fs::read_to_string(&utils).unwrap().lines().count() as u32;
    client.did_change_insert(&utils, last_line, 0, "(core/add 9 9)\n");

    let (line, ch) = position_of(&core, "add");
    let result = client.rename(&core, line, ch, "plus");
    let changes = result["changes"].as_object().unwrap();
    let utils_edits = changes
        .iter()
        .find(|(uri, _)| uri.ends_with("/src/utils.clj"))
        .unwrap()
        .1
        .as_array()
        .unwrap();

    assert_eq!(
        utils_edits.len(),
        2,
        "saved + unsaved usage must both be edited: {:?}",
        utils_edits
    );
    assert!(
        utils_edits
            .iter()
            .any(|e| e["range"]["start"]["line"] == json!(last_line)),
        "unsaved usage line missing from edits"
    );
}

#[test]
fn test_e2e_watched_files_keep_index_fresh() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    // Consumer of a namespace that doesn't exist yet
    let consumer = root.join("src/uses_fresh.clj");
    std::fs::write(
        &consumer,
        "(ns uses-fresh\n  (:require [simple.fresh :as fr]))\n\n(fr/fresh-fn 1)\n",
    )
    .unwrap();
    client.did_open(&consumer);
    let (line, ch) = position_of(&consumer, "fr/fresh-fn");

    assert!(
        client.goto_definition(&consumer, line, ch).is_null(),
        "definition should not resolve before the file exists"
    );

    // Simulate `git pull` creating the file (no editor save involved)
    let fresh = root.join("src/fresh.clj");
    std::fs::write(&fresh, "(ns simple.fresh)\n\n(defn fresh-fn [x] x)\n").unwrap();
    client.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": format!("file://{}", fresh.display()), "type": 1 }] }),
    );

    // Notifications are processed asynchronously — poll
    let deadline = Instant::now() + TIMEOUT;
    let mut result = Value::Null;
    while Instant::now() < deadline {
        result = client.goto_definition(&consumer, line, ch);
        if !result.is_null() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let uri = result["uri"]
        .as_str()
        .expect("definition after Created event");
    assert!(uri.ends_with("/src/fresh.clj"), "got {}", uri);

    // Simulate the file being deleted
    std::fs::remove_file(&fresh).unwrap();
    client.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": format!("file://{}", fresh.display()), "type": 3 }] }),
    );
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let result = client.goto_definition(&consumer, line, ch);
        if result.is_null() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "definition still resolves after Deleted event"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn test_e2e_definition_after_in_memory_edit() {
    // Type new code without saving: didChange must keep the in-memory
    // document in sync so navigation works from unsaved edits.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let utils = root.join("src/utils.clj");
    let core = root.join("src/core.clj");
    client.did_open(&utils);

    let last_line = std::fs::read_to_string(&utils).unwrap().lines().count() as u32;
    client.did_change_insert(&utils, last_line, 0, "(core/multiply 3 4)\n");

    let result = client.goto_definition(&utils, last_line, 8);

    assert!(
        !result.is_null(),
        "goto-definition on unsaved edit returned null"
    );
    let uri = result["uri"].as_str().expect("expected Location");
    assert!(
        uri.ends_with("/src/core.clj"),
        "expected core.clj, got {}",
        uri
    );
    let (def_line, _) = position_of(&core, "defn multiply");
    assert_eq!(result["range"]["start"]["line"], json!(def_line));
}

/// A `(line, character)` pair in LSP coordinates.
type Pos = (u32, u32);
/// One incremental change: `(start, end, text)`.
type Edit = (Pos, Pos, String);

/// Applies one LSP range edit to `text`: ASCII-only, so UTF-16 columns are
/// byte columns. The tests' own model of what the server's buffer holds.
fn apply_range_edit(text: &str, start: (u32, u32), end: (u32, u32), insert: &str) -> String {
    let offset = |(line, ch): (u32, u32)| {
        let mut off = 0;
        for (i, l) in text.split_inclusive('\n').enumerate() {
            if i == line as usize {
                return off + ch as usize;
            }
            off += l.len();
        }
        off + ch as usize
    };
    let (from, to) = (offset(start), offset(end));
    format!("{}{}{}", &text[..from], insert, &text[to..])
}

/// Diagnostics as `(code, message, range)` triples, so two publishes for
/// different files can be compared.
fn diagnostic_shapes(params: &Value) -> Vec<(String, String, Value)> {
    params["diagnostics"]
        .as_array()
        .expect("diagnostics array")
        .iter()
        .map(|d| {
            (
                d["code"].as_str().unwrap_or_default().to_string(),
                d["message"].as_str().unwrap_or_default().to_string(),
                d["range"].clone(),
            )
        })
        .collect()
}

#[test]
fn test_e2e_diagnostics_stable_across_edits() {
    // Every lint pass after the first runs on the incrementally updated tree,
    // never on a fresh parse. So after each of these edits the diagnostics must
    // be exactly what a server that opens the same text cold publishes —
    // through unbalanced intermediate states, an added-then-removed unused
    // require, and lines shifting under the ns form.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();
    let reference = setup_project();
    let reference_root = reference.path().canonicalize().unwrap();

    let mut client = LspClient::start_verbose(&root);
    client.initialize(&root);
    let mut fresh = LspClient::start(&reference_root);
    fresh.initialize(&reference_root);

    let scratch = root.join("src/scratch.clj");
    let mut text = "(ns simple.scratch\n  (:require [simple.helpers :as helpers]))\n\n\
                    (defn run []\n  (helpers/greet \"hi\"))\n"
        .to_string();
    std::fs::write(&scratch, &text).unwrap();
    client.did_open(&scratch);
    client.wait_for_diagnostics("/src/scratch.clj");

    // Twenty edits the way an editor sends them: a second require (unused for
    // now), a fn typed in chunks that uses it, then everything unwound with a
    // line deleted above it all so every following line shifts.
    let append = |text: &str, chunk: &str| -> Edit {
        let lines: Vec<&str> = text.split('\n').collect();
        let last = (lines.len() - 1) as u32;
        let at = (last, lines[last as usize].len() as u32);
        (at, at, chunk.to_string())
    };
    let mut edits: Vec<Edit> = Vec::new();
    let mut model = text.clone();
    let mut plan = |edit: Edit, model: &mut String| {
        *model = apply_range_edit(model, edit.0, edit.1, &edit.2);
        edits.push(edit);
    };
    plan(
        (
            (1, 40),
            (1, 40),
            "\n            [clojure.string :as str]".to_string(),
        ),
        &mut model,
    );
    for chunk in [
        "(defn ",
        "shout ",
        "[s]",
        "\n  (str/",
        "upper-case",
        " s",
        ")",
        ")\n",
    ] {
        let edit = append(&model, chunk);
        plan(edit, &mut model);
    }
    assert!(
        model.ends_with("(defn shout [s]\n  (str/upper-case s))\n"),
        "{model}"
    );
    let unwind: [(&str, Pos, Pos); 11] = [
        ("nil", (7, 2), (7, 20)),                // drop the usage: `s` is unused
        ("", (6, 13), (6, 14)),                  // `[s]` -> `[]`
        ("(defn shout [] nil)", (6, 0), (7, 6)), // one-line fn
        ("", (3, 0), (4, 0)),                    // delete the blank line under the ns
        ("", (1, 40), (2, 36)),                  // remove `[clojure.string :as str]`
        ("", (4, 0), (5, 0)),                    // delete the `shout` line
        (" ", (3, 2), (3, 2)),                   // a whitespace-only change
        ("", (3, 2), (3, 3)),                    // and back
        ("simple.scratch2", (0, 4), (0, 18)),    // rename the ns
        ("simple.scratch", (0, 4), (0, 19)),     // and back
        ("(def leftover 1)\n", (4, 0), (4, 0)),  // and one more form
    ];
    for (insert, start, end) in unwind {
        plan((start, end, insert.to_string()), &mut model);
    }
    assert_eq!(edits.len(), 20);

    for (i, (start, end, insert)) in edits.iter().enumerate() {
        let version = (i + 2) as i64;
        text = apply_range_edit(&text, *start, *end, insert);
        client.clear_notifications();
        client.did_change_range(&scratch, version, *start, *end, insert);
        let live = client.wait_for_diagnostics("/src/scratch.clj");
        assert_eq!(
            live["version"],
            json!(version),
            "publish carries the edit's version"
        );

        // A cold open of the same text, on a server that never saw the edits.
        let path = reference_root.join(format!("src/step_{i}.clj"));
        std::fs::write(&path, &text).unwrap();
        fresh.did_open(&path);
        let expected = fresh.wait_for_diagnostics(&format!("/src/step_{i}.clj"));
        assert_eq!(
            diagnostic_shapes(&live),
            diagnostic_shapes(&expected),
            "edit {i} ({start:?}..{end:?} {insert:?}) on:\n{text}"
        );
    }
    assert_eq!(text, model);

    // And the passes really came from the cache: none of them parsed.
    LspClient::wait_for_server_log(&root, "parsed=0");
    assert!(
        !LspClient::server_log(&root).contains("parsed=1"),
        "a lint pass parsed the buffer instead of using the cached tree"
    );
}

#[test]
fn test_e2e_definition_after_edits() {
    // Position requests read the cached tree too. After edits that shift lines
    // and change the form under the cursor, definition must land on the right
    // symbol — cross-file through the index and a local through the tree.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let utils = root.join("src/utils.clj");
    let core = root.join("src/core.clj");
    client.did_open(&utils);

    let last_line = std::fs::read_to_string(&utils).unwrap().lines().count() as u32;
    let mut version = 2;
    let mut edit = |client: &mut LspClient, start, end, text: &str| {
        client.did_change_range(&utils, version, start, end, text);
        version += 1;
    };
    // Type a new fn in pieces...
    edit(
        &mut client,
        (last_line, 0),
        (last_line, 0),
        "(defn scale [factor xs]\n",
    );
    edit(
        &mut client,
        (last_line + 1, 0),
        (last_line + 1, 0),
        "  (map #(core/multiply factor %) xs))\n",
    );
    // ...then insert two lines above it, so its lines move.
    edit(&mut client, (0, 0), (0, 0), ";; header\n;; more\n");
    // ...and change the form itself: `multiply` -> `add`, then back.
    let fn_line = last_line + 3;
    edit(&mut client, (fn_line, 14), (fn_line, 22), "add");
    edit(&mut client, (fn_line, 14), (fn_line, 17), "multiply");

    let result = client.goto_definition(&utils, fn_line, 16);
    let uri = result["uri"].as_str().expect("expected a Location");
    assert!(uri.ends_with("/src/core.clj"), "got {uri}");
    let (def_line, _) = position_of(&core, "defn multiply");
    assert_eq!(result["range"]["start"]["line"], json!(def_line));

    // The local `factor` resolves to its param binding, two lines up.
    let result = client.goto_definition(&utils, fn_line, 25);
    let uri = result["uri"].as_str().expect("expected a Location");
    assert!(uri.ends_with("/src/utils.clj"), "got {uri}");
    assert_eq!(result["range"]["start"]["line"], json!(fn_line - 1));
    assert_eq!(result["range"]["start"]["character"], json!(13));
}

#[test]
fn test_e2e_jar_definition_and_content() {
    // Library symbol: definition must return a jar: URI, and
    // workspace/textDocumentContent must serve the source behind it.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let jar_path = root.join("mylib.jar");
    let jar_file = std::fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(jar_file);
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("mylib/util.clj", opts).unwrap();
    zip.write_all(b"(ns mylib.util)\n\n(defn helper\n  \"Does helping.\"\n  [x]\n  x)\n")
        .unwrap();
    zip.finish().unwrap();

    let cpcache = root.join(".cpcache");
    std::fs::create_dir_all(&cpcache).unwrap();
    std::fs::write(cpcache.join("1.cp"), jar_path.display().to_string()).unwrap();

    let consumer = root.join("src/uses_lib.clj");
    std::fs::write(
        &consumer,
        "(ns uses-lib\n  (:require [mylib.util :as u]))\n\n(u/helper 42)\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");
    client.did_open(&consumer);

    let (line, ch) = position_of(&consumer, "u/helper");
    let result = client.goto_definition(&consumer, line, ch);

    assert!(!result.is_null(), "goto-definition into JAR returned null");
    let uri = result["uri"].as_str().expect("expected Location");
    assert!(uri.starts_with("jar:file://"), "expected jar: URI: {}", uri);
    assert!(uri.ends_with("!/mylib/util.clj"), "wrong entry: {}", uri);

    let content = client.text_document_content(uri);
    let text = content["text"].as_str().expect("expected text");
    assert!(text.contains("(defn helper"), "wrong JAR content: {}", text);
}

#[test]
fn test_e2e_gitlib_directory_dependency() {
    // Git deps (and :local/root deps) appear on the classpath as source
    // *directories* (~/.gitlibs/libs/...), not JARs. They must be indexed
    // and navigable via plain file: URIs.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let libdir = tempfile::TempDir::new().unwrap();
    let lib_src = libdir.path().join("src");
    std::fs::create_dir_all(lib_src.join("gitlib")).unwrap();
    let lib_file = lib_src.join("gitlib/util.clj");
    std::fs::write(
        &lib_file,
        "(ns gitlib.util)\n\n(defn helper\n  \"From a git dep.\"\n  [x]\n  x)\n",
    )
    .unwrap();

    let cpcache = root.join(".cpcache");
    std::fs::create_dir_all(&cpcache).unwrap();
    std::fs::write(cpcache.join("1.cp"), lib_src.display().to_string()).unwrap();

    let consumer = root.join("src/uses_gitlib.clj");
    std::fs::write(
        &consumer,
        "(ns uses-gitlib\n  (:require [gitlib.util :as u]))\n\n(u/helper 42)\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");
    client.did_open(&consumer);

    let (line, ch) = position_of(&consumer, "u/helper");
    let result = client.goto_definition(&consumer, line, ch);

    assert!(
        !result.is_null(),
        "goto-definition into a directory dep returned null"
    );
    let uri = result["uri"].as_str().expect("expected Location");
    let expected = format!("file://{}", lib_file.canonicalize().unwrap().display());
    assert_eq!(uri, expected, "expected file: URI into the lib directory");
    let (def_line, _) = position_of(&lib_file, "defn helper");
    assert_eq!(result["range"]["start"]["line"], json!(def_line));
}

/// A `:projects` entry disabling the root project must fully suppress stage-3
/// resolution: no "resolving classpath" attempt, just the stage-2 outcome
/// (here the no-classpath warning, since the fixture has no `.cpcache`).
#[test]
fn test_e2e_classpath_cli_disabled_by_config() {
    let project = tempfile::TempDir::new().unwrap();
    let root = project.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join(".clj-pulse")).unwrap();
    std::fs::write(root.join("deps.edn"), "{:paths [\"src\"]}\n").unwrap();
    std::fs::write(
        root.join(".clj-pulse/config.edn"),
        "{:projects [{:path \".\" :classpath {:enabled false}}]}\n",
    )
    .unwrap();
    std::fs::write(root.join("src/app.clj"), "(ns app)\n\n(defn go [] 1)\n").unwrap();

    // No kill-switch env var: the config file itself is what disables stage 3.
    let mut client = LspClient::start_with_classpath_cli(&root);
    client.initialize(&root);
    // The zero-entry stage-2 path emits the warning (never "library indexing
    // complete"); by then any stage-3 attempt would already have logged.
    client.wait_for_log("no classpath found");

    let resolving = client.notifications.iter().any(|m| {
        m["method"] == "window/logMessage"
            && m["params"]["message"]
                .as_str()
                .map(|s| s.contains("resolving classpath"))
                .unwrap_or(false)
    });
    assert!(
        !resolving,
        ":classpath {{:enabled false}} must suppress the clojure CLI run"
    );
}

/// The headline scenario: a dep declared only under a `:test` alias's
/// `:extra-deps`, with `.cpcache` primed by plain `clojure -Spath` (main deps
/// only — the original bug). Stage 3 must resolve `-A:dev:test` and make the
/// alias-only jar navigable. Requires the `clojure` CLI, so ignored by
/// default: `cargo test --test test_e2e -- --ignored`
#[test]
#[ignore = "requires clojure CLI (downloads deps on first run)"]
fn test_e2e_alias_classpath_navigation() {
    let project = tempfile::TempDir::new().unwrap();
    let root = project.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("deps.edn"),
        r#"{:paths ["src"]
 :aliases {:test {:extra-deps {org.clojure/data.json {:mvn/version "2.5.0"}}}}}
"#,
    )
    .unwrap();
    let app = root.join("src/app.clj");
    std::fs::write(
        &app,
        "(ns app\n  (:require [clojure.data.json :as json]))\n\n(json/write-str {:a 1})\n",
    )
    .unwrap();

    // Prime the cache the way a plain REPL start would — without the :test
    // alias, so data.json is NOT on the stage-2 classpath.
    let out = std::process::Command::new("clojure")
        .args(["-Spath"])
        .current_dir(&root)
        .output()
        .expect("clojure CLI not available");
    assert!(out.status.success(), "clojure -Spath failed");

    let mut client = LspClient::start_with_classpath_cli(&root);
    client.initialize(&root);
    // Stage 3 may download the alias-only dep on a cold machine; give it the
    // resolver's own budget rather than the harness default.
    client.wait_for_log_within("full classpath indexed", Duration::from_secs(300));
    client.did_open(&app);

    let (line, ch) = position_of(&app, "json/write-str");
    let result = client.goto_definition(&app, line, ch);

    assert!(
        !result.is_null(),
        "goto-definition into the :test-alias dep returned null"
    );
    let uri = result["uri"].as_str().expect("expected Location");
    assert!(
        uri.starts_with("jar:file://") && uri.ends_with("!/clojure/data/json.clj"),
        "unexpected URI: {}",
        uri
    );

    let content = client.text_document_content(uri);
    let text = content["text"].as_str().expect("expected text");
    assert!(text.contains("(defn write-str"), "wrong JAR content");
}

/// Full realistic scenario against a real Maven classpath. Requires the
/// `clojure` CLI and network/m2 access, so it is ignored by default:
/// `cargo test --test test_e2e -- --ignored`
#[test]
#[ignore = "requires clojure CLI (downloads deps on first run)"]
fn test_e2e_real_classpath_navigation() {
    let project = tempfile::TempDir::new().unwrap();
    let root = project.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("deps.edn"),
        r#"{:paths ["src"]
 :deps {org.clojure/data.json {:mvn/version "2.5.0"}}}
"#,
    )
    .unwrap();
    let app = root.join("src/app.clj");
    std::fs::write(
        &app,
        "(ns app\n  (:require [clojure.data.json :as json]))\n\n(json/write-str {:a 1})\n",
    )
    .unwrap();

    // Produce .cpcache the same way a real project gets one
    let out = std::process::Command::new("clojure")
        .args(["-Spath"])
        .current_dir(&root)
        .output()
        .expect("clojure CLI not available");
    assert!(out.status.success(), "clojure -Spath failed");

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");
    client.did_open(&app);

    let (line, ch) = position_of(&app, "json/write-str");
    let result = client.goto_definition(&app, line, ch);

    assert!(
        !result.is_null(),
        "goto-definition into data.json returned null"
    );
    let uri = result["uri"].as_str().expect("expected Location");
    assert!(
        uri.starts_with("jar:file://") && uri.ends_with("!/clojure/data/json.clj"),
        "unexpected URI: {}",
        uri
    );

    let content = client.text_document_content(uri);
    let text = content["text"].as_str().expect("expected text");
    assert!(text.contains("(defn write-str"), "wrong JAR content");
}

#[test]
fn test_e2e_paths_inside_alias_do_not_break_indexing() {
    // A deps.edn whose only `:paths` lives inside an alias (tools.build
    // convention) must not be mistaken for the project's source paths —
    // otherwise src/ is never indexed and all navigation breaks.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();
    std::fs::write(
        root.join("deps.edn"),
        r#"{:deps {org.clojure/clojure {:mvn/version "1.11.1"}}
 :aliases {:build {:paths ["build"]
                   :deps {io.github.clojure/tools.build {:mvn/version "0.9.6"}}}}}
"#,
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let utils = root.join("src/utils.clj");
    client.did_open(&utils);

    let (line, ch) = position_of(&utils, "core/add");
    let result = client.goto_definition(&utils, line, ch);

    assert!(
        !result.is_null(),
        "goto-definition returned null: src/ was not indexed (alias :paths hijacked source-path detection)"
    );
    let uri = result["uri"].as_str().expect("expected Location");
    assert!(
        uri.ends_with("/src/core.clj"),
        "expected core.clj, got {}",
        uri
    );
}

#[test]
fn test_e2e_project_symbols_not_shadowed_by_jars() {
    // A classpath JAR containing the same namespace as the project (e.g. an
    // older version of the project installed in ~/.m2) must not hijack
    // navigation for project files.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let jar_path = root.join("old-simple.jar");
    let jar_file = std::fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(jar_file);
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("simple/core.clj", opts).unwrap();
    zip.write_all(b"(ns simple.core)\n\n(defn add [a b] (+ a b))\n")
        .unwrap();
    zip.finish().unwrap();

    let cpcache = root.join(".cpcache");
    std::fs::create_dir_all(&cpcache).unwrap();
    std::fs::write(cpcache.join("1.cp"), jar_path.display().to_string()).unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");

    let utils = root.join("src/utils.clj");
    client.did_open(&utils);

    let (line, ch) = position_of(&utils, "core/add");
    let result = client.goto_definition(&utils, line, ch);

    assert!(!result.is_null(), "goto-definition returned null");
    let uri = result["uri"].as_str().expect("expected Location");
    assert!(
        uri.ends_with("/src/core.clj"),
        "project symbol shadowed by JAR: {}",
        uri
    );
}

#[test]
fn test_e2e_transitive_definition_jar_to_jar() {
    // Navigate project → mylib.core (a JAR entry), then from *inside* that JAR
    // entry on into its own dependency mylib.util — the transitive hop.
    let (_project, root) = two_ns_jar_project();
    let consumer = root.join("src/uses_lib.clj");

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");
    client.did_open(&consumer);

    let (line, ch) = position_of(&consumer, "core/run");
    let core_loc = client.goto_definition(&consumer, line, ch);
    let core_uri = core_loc["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("no jar nav into mylib.core: {}", core_loc))
        .to_string();
    assert!(
        core_uri.ends_with("!/mylib/core.clj"),
        "expected jar nav into mylib.core, got {}",
        core_uri
    );

    // Open the JAR entry the way the editor would, then navigate from within it.
    let core_src = client.text_document_content(&core_uri)["text"]
        .as_str()
        .expect("jar content")
        .to_string();
    client.did_open_uri(&core_uri, &core_src);

    let (line, ch) = position_in_text(&core_src, "util/helper");
    let util_loc = client.goto_definition_uri(&core_uri, line, ch);
    let util_uri = util_loc["uri"].as_str().unwrap_or_default();
    assert!(
        util_uri.ends_with("!/mylib/util.clj"),
        "expected transitive nav into mylib.util, got {}",
        util_loc
    );
}

#[test]
fn test_e2e_references_from_inside_library_file() {
    // Find-references invoked from the `helper` declaration inside the JAR file
    // returns the project usage, the lib→lib usage in mylib.core, and the
    // declaration itself.
    let (_project, root) = two_ns_jar_project();
    let consumer = root.join("src/uses_lib.clj");

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");
    client.did_open(&consumer);

    // Open both JAR entries as editor docs.
    let (l, c) = position_of(&consumer, "util/helper");
    let util_uri = client.goto_definition(&consumer, l, c)["uri"]
        .as_str()
        .unwrap()
        .to_string();
    let util_src = client.text_document_content(&util_uri)["text"]
        .as_str()
        .unwrap()
        .to_string();
    client.did_open_uri(&util_uri, &util_src);

    let (l, c) = position_of(&consumer, "core/run");
    let core_uri = client.goto_definition(&consumer, l, c)["uri"]
        .as_str()
        .unwrap()
        .to_string();
    let core_src = client.text_document_content(&core_uri)["text"]
        .as_str()
        .unwrap()
        .to_string();
    client.did_open_uri(&core_uri, &core_src);

    let (line, ch) = position_in_text(&util_src, "helper");
    let result = client.references_uri(&util_uri, line, ch, true);
    let uris: Vec<&str> = result
        .as_array()
        .expect("references array")
        .iter()
        .filter_map(|loc| loc["uri"].as_str())
        .collect();

    assert!(
        uris.iter().any(|u| u.ends_with("/src/uses_lib.clj")),
        "expected the project usage, got {:?}",
        uris
    );
    assert!(
        uris.iter().any(|u| u.ends_with("!/mylib/core.clj")),
        "expected the lib→lib usage in mylib.core, got {:?}",
        uris
    );
    assert!(
        uris.iter().any(|u| u.ends_with("!/mylib/util.clj")),
        "expected the declaration in mylib.util, got {:?}",
        uris
    );
}

#[test]
fn test_e2e_hover_from_inside_library_file() {
    // Hovering a symbol inside a JAR file resolves it through that file's own
    // requires and shows its docs.
    let (_project, root) = two_ns_jar_project();
    let consumer = root.join("src/uses_lib.clj");

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");
    client.did_open(&consumer);

    let (l, c) = position_of(&consumer, "core/run");
    let core_uri = client.goto_definition(&consumer, l, c)["uri"]
        .as_str()
        .unwrap()
        .to_string();
    let core_src = client.text_document_content(&core_uri)["text"]
        .as_str()
        .unwrap()
        .to_string();
    client.did_open_uri(&core_uri, &core_src);

    let (line, ch) = position_in_text(&core_src, "util/helper");
    let hover = client.hover_uri(&core_uri, line, ch);
    let value = hover["contents"]["value"].as_str().unwrap_or_default();
    assert!(
        value.contains("helper") && value.contains("mylib.util"),
        "expected hover for mylib.util/helper from inside the JAR, got {}",
        hover
    );
}

#[test]
fn test_e2e_dependency_contents_serves_jar_source() {
    // clojure-lsp's `clojure/dependencyContents` returns the raw entry text for
    // a jar: URI — the request Calva issues to open a navigation target.
    let (_project, root) = two_ns_jar_project();
    let consumer = root.join("src/uses_lib.clj");

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");
    client.did_open(&consumer);

    let (l, c) = position_of(&consumer, "core/run");
    let core_uri = client.goto_definition(&consumer, l, c)["uri"]
        .as_str()
        .unwrap()
        .to_string();

    let contents = client.dependency_contents(&core_uri);
    let text = contents.as_str().unwrap_or_default();
    assert!(
        text.contains("(ns mylib.core") && text.contains("util/helper"),
        "expected raw mylib.core source from dependencyContents, got {:?}",
        contents
    );
}

#[test]
fn test_e2e_definition_from_percent_encoded_jar_uri() {
    // VS Code / Calva re-encode the jar URI before sending didOpen / definition
    // (`file:`→`file%3A`, `!`→`%21`). The server must still resolve from it —
    // this is the real-world Calva failure mode.
    let (_project, root) = two_ns_jar_project();
    let consumer = root.join("src/uses_lib.clj");

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");
    client.did_open(&consumer);

    let (l, c) = position_of(&consumer, "core/run");
    let clean = client.goto_definition(&consumer, l, c)["uri"]
        .as_str()
        .unwrap()
        .to_string();
    let encoded = clean.replace("file:", "file%3A").replace("!/", "%21/");
    assert!(
        encoded.contains("file%3A") && encoded.contains("%21/"),
        "encoding sanity check failed: {}",
        encoded
    );

    let src = client.text_document_content(&clean)["text"]
        .as_str()
        .unwrap()
        .to_string();
    client.did_open_uri(&encoded, &src);

    let (line, ch) = position_in_text(&src, "util/helper");
    let loc = client.goto_definition_uri(&encoded, line, ch);
    assert!(
        loc["uri"]
            .as_str()
            .unwrap_or_default()
            .ends_with("!/mylib/util.clj"),
        "expected nav from a percent-encoded jar buffer, got {}",
        loc
    );
}

#[test]
fn test_e2e_navigate_to_private_fn_in_jar() {
    // Private (`defn-`) functions in library files are navigable from inside the
    // library source. `caller` calls the private `secret` unqualified.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let jar_path = root.join("mylib.jar");
    let jar_file = std::fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(jar_file);
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("mylib/util.clj", opts).unwrap();
    zip.write_all(b"(ns mylib.util)\n\n(defn- secret [x] x)\n\n(defn caller [x] (secret x))\n")
        .unwrap();
    zip.finish().unwrap();

    let cpcache = root.join(".cpcache");
    std::fs::create_dir_all(&cpcache).unwrap();
    std::fs::write(cpcache.join("1.cp"), jar_path.display().to_string()).unwrap();

    let consumer = root.join("src/uses_lib.clj");
    std::fs::write(
        &consumer,
        "(ns uses-lib\n  (:require [mylib.util :as u]))\n\n(u/caller 1)\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");
    client.did_open(&consumer);

    let (l, c) = position_of(&consumer, "u/caller");
    let util_uri = client.goto_definition(&consumer, l, c)["uri"]
        .as_str()
        .unwrap()
        .to_string();
    let util_src = client.text_document_content(&util_uri)["text"]
        .as_str()
        .unwrap()
        .to_string();
    client.did_open_uri(&util_uri, &util_src);

    let (line, ch) = position_in_text(&util_src, "(secret x)");
    let loc = client.goto_definition_uri(&util_uri, line, ch);
    assert!(
        loc["uri"]
            .as_str()
            .unwrap_or_default()
            .ends_with("!/mylib/util.clj"),
        "expected nav to a private fn in the lib, got {}",
        loc
    );
    assert_eq!(
        loc["range"]["start"]["line"].as_u64(),
        Some(2),
        "expected the `(defn- secret ...)` line, got {}",
        loc
    );
}

#[test]
fn test_e2e_navigate_into_impl_namespace_from_jar() {
    // Library-internal `.impl` namespaces are indexed, so navigating from one
    // library file into the lib's `.impl` namespace works (the claypoole
    // `impl/validate-future-pool` case).
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let jar_path = root.join("mylib.jar");
    let jar_file = std::fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(jar_file);
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("mylib/impl.clj", opts).unwrap();
    zip.write_all(b"(ns mylib.impl)\n\n(defn validate [x] x)\n")
        .unwrap();
    zip.start_file("mylib/core.clj", opts).unwrap();
    zip.write_all(
        b"(ns mylib.core\n  (:require [mylib.impl :as impl]))\n\n(defn run [x] (impl/validate x))\n",
    )
    .unwrap();
    zip.finish().unwrap();

    let cpcache = root.join(".cpcache");
    std::fs::create_dir_all(&cpcache).unwrap();
    std::fs::write(cpcache.join("1.cp"), jar_path.display().to_string()).unwrap();

    let consumer = root.join("src/uses_lib.clj");
    std::fs::write(
        &consumer,
        "(ns uses-lib\n  (:require [mylib.core :as core]))\n\n(core/run 1)\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");
    client.did_open(&consumer);

    let (l, c) = position_of(&consumer, "core/run");
    let core_uri = client.goto_definition(&consumer, l, c)["uri"]
        .as_str()
        .unwrap()
        .to_string();
    let core_src = client.text_document_content(&core_uri)["text"]
        .as_str()
        .unwrap()
        .to_string();
    client.did_open_uri(&core_uri, &core_src);

    let (line, ch) = position_in_text(&core_src, "impl/validate");
    let loc = client.goto_definition_uri(&core_uri, line, ch);
    assert!(
        loc["uri"]
            .as_str()
            .unwrap_or_default()
            .ends_with("!/mylib/impl.clj"),
        "expected nav into the .impl namespace, got {}",
        loc
    );
}

#[test]
fn test_e2e_definition_bare_same_ns_symbol_in_jar() {
    // The reported claypoole case: inside a JAR file, go-to-definition on a
    // BARE, same-namespace symbol (`completable-future-call`), not a qualified
    // cross-ns ref. `caller` calls `helper` unqualified, both in mylib.util.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let jar_path = root.join("mylib.jar");
    let jar_file = std::fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(jar_file);
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("mylib/util.clj", opts).unwrap();
    zip.write_all(b"(ns mylib.util)\n\n(defn helper [x] x)\n\n(defn caller [x] (helper x))\n")
        .unwrap();
    zip.finish().unwrap();

    let cpcache = root.join(".cpcache");
    std::fs::create_dir_all(&cpcache).unwrap();
    std::fs::write(cpcache.join("1.cp"), jar_path.display().to_string()).unwrap();

    let consumer = root.join("src/uses_lib.clj");
    std::fs::write(
        &consumer,
        "(ns uses-lib\n  (:require [mylib.util :as u]))\n\n(u/caller 1)\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");
    client.did_open(&consumer);

    // Into the JAR, then open it the way the editor would.
    let (l, c) = position_of(&consumer, "u/caller");
    let util_uri = client.goto_definition(&consumer, l, c)["uri"]
        .as_str()
        .unwrap()
        .to_string();
    let util_src = client.text_document_content(&util_uri)["text"]
        .as_str()
        .unwrap()
        .to_string();
    client.did_open_uri(&util_uri, &util_src);

    // Cursor on the bare `helper` inside `(helper x)` → its definition above.
    let (line, ch) = position_in_text(&util_src, "(helper x)");
    let loc = client.goto_definition_uri(&util_uri, line, ch);
    let uri = loc["uri"].as_str().unwrap_or_default();
    assert!(
        uri.ends_with("!/mylib/util.clj"),
        "expected same-file nav for a bare same-ns symbol, got {}",
        loc
    );
    assert_eq!(
        loc["range"]["start"]["line"].as_u64(),
        Some(2),
        "expected the `(defn helper ...)` line, got {}",
        loc
    );
}

#[test]
fn test_e2e_rename_rejected_from_library_file() {
    // Navigation/inspection work from a JAR buffer, but rename must not: a
    // library file is read-only, and the fqn-only resolver could otherwise edit
    // a project symbol that shadows the library one.
    let (_project, root) = two_ns_jar_project();
    let consumer = root.join("src/uses_lib.clj");

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");
    client.did_open(&consumer);

    let (l, c) = position_of(&consumer, "util/helper");
    let util_uri = client.goto_definition(&consumer, l, c)["uri"]
        .as_str()
        .unwrap()
        .to_string();
    let util_src = client.text_document_content(&util_uri)["text"]
        .as_str()
        .unwrap()
        .to_string();
    client.did_open_uri(&util_uri, &util_src);

    let (line, ch) = position_in_text(&util_src, "helper");
    let msg = client.rename_uri(&util_uri, line, ch, "renamed");
    let err = msg
        .get("error")
        .unwrap_or_else(|| panic!("rename from a JAR buffer should be rejected, got {}", msg));
    assert!(
        err["message"]
            .as_str()
            .unwrap_or_default()
            .contains("library file"),
        "expected a library-file rejection, got {}",
        err
    );
}

#[test]
fn test_e2e_integrant_goto_definition_from_config() {
    // The headline feature: from a namespaced keyword in an Integrant
    // `config.edn` system map, navigate to its `(defmethod ig/init-key ::db …)`.
    let project = setup_named("integrant_project");
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("Indexed");

    let config = root.join("resources/config.edn");
    let db = root.join("src/readx/db.clj");
    client.did_open(&config);

    // Cursor on the `:readx.db/db` map key.
    let (line, ch) = position_of(&config, ":readx.db/db");
    let result = client.goto_definition(&config, line, ch);
    let uri = result["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("no def for :readx.db/db: {}", result));
    assert!(uri.ends_with("/src/readx/db.clj"), "got {}", uri);

    // Lands on the ig/init-key defmethod line — not assert-key or halt-key!.
    let (init_line, _) = position_of(&db, "ig/init-key");
    assert_eq!(result["range"]["start"]["line"], json!(init_line));
}

#[test]
fn test_e2e_integrant_references_span_defmethods_and_config() {
    // References on the component keyword reach every lifecycle defmethod plus
    // the config-map key and the `#ig/ref`.
    let project = setup_named("integrant_project");
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("Indexed");

    let db = root.join("src/readx/db.clj");
    client.did_open(&db);

    // First `::db` in db.clj is the assert-key dispatch (an occurrence); it
    // resolves to the same `:readx.db/db` as the init-key definition.
    let (line, ch) = position_of(&db, "::db");
    let result = client.references(&db, line, ch, true);
    let locs = result
        .as_array()
        .unwrap_or_else(|| panic!("references returned null: {}", result));

    // 3 in db.clj (init-key declaration + assert-key + halt-key! occurrences),
    // 2 in resources/config.edn (the map key + the `#ig/ref` value).
    assert_eq!(locs.len(), 5, "locs: {:?}", locs);
    let uris: Vec<&str> = locs.iter().filter_map(|l| l["uri"].as_str()).collect();
    assert_eq!(
        uris.iter()
            .filter(|u| u.ends_with("/src/readx/db.clj"))
            .count(),
        3,
        "db.clj locations: {:?}",
        locs
    );
    assert_eq!(
        uris.iter()
            .filter(|u| u.ends_with("/resources/config.edn"))
            .count(),
        2,
        "config.edn locations: {:?}",
        locs
    );
}

#[test]
fn test_e2e_keyword_does_not_navigate_to_same_named_var() {
    // A namespaced keyword must never goto-def to a same-named var. With no
    // keyword definition, goto-def yields nothing rather than a wrong jump
    // (regression: `::counter` used to land on `(defn- counter …)`).
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let f = root.join("src/metrics.clj");
    std::fs::write(
        &f,
        "(ns app.metrics)\n(defn- counter [] 1)\n(def m {::counter (counter)})\n(::counter m)\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("Indexed");
    client.did_open(&f);

    // Cursor on the `::counter` keyword (the map key on line 2).
    let (line, ch) = position_of(&f, "::counter");
    let result = client.goto_definition(&f, line, ch);
    assert!(
        result.is_null(),
        "keyword must not navigate to the same-named var, got {}",
        result
    );
}

#[test]
fn test_e2e_unqualified_keyword_does_not_navigate_to_var() {
    // Same guard for an *unqualified* keyword: `:counter` must not goto-def to
    // a same-named var. (resolve_fqn_at returns nothing for unqualified
    // keywords, so this relies on the keyword-token check, not the fqn.)
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let f = root.join("src/unq.clj");
    std::fs::write(
        &f,
        "(ns app.unq)\n(defn- counter [] 1)\n(def m {:counter (counter)})\n(:counter m)\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("Indexed");
    client.did_open(&f);

    // Cursor on an unqualified `:counter` keyword (the first is the map key).
    let (line, ch) = position_of(&f, ":counter");
    let result = client.goto_definition(&f, line, ch);
    assert!(
        result.is_null(),
        "unqualified keyword must not navigate to the same-named var, got {}",
        result
    );
}

#[test]
fn test_e2e_keyword_navigation_with_cursor_on_colon() {
    // The whole-keyword nav must work even with the cursor on the leading `:`
    // of the keyword, not just on the name part.
    let project = setup_named("integrant_project");
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("Indexed");

    let config = root.join("resources/config.edn");
    client.did_open(&config);

    // Column of the leading ':' of `:readx.db/db` (its first appearance).
    let text = std::fs::read_to_string(&config).unwrap();
    let (line, col) = text
        .lines()
        .enumerate()
        .find_map(|(i, l)| l.find(":readx.db/db").map(|c| (i as u32, c as u32)))
        .expect(":readx.db/db not found");

    let result = client.goto_definition(&config, line, col);
    let uri = result["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("cursor on the ':' should still resolve, got {}", result));
    assert!(uri.ends_with("/src/readx/db.clj"), "got {}", uri);
}

#[test]
fn test_e2e_integrant_refless_config_navigates() {
    // A ref-less Integrant config (no #ig/ref anywhere) must still navigate from
    // a component key to its `ig/init-key` defmethod.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let db = root.join("src/sys/db.clj");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    std::fs::write(
        &db,
        "(ns sys.db\n  (:require [integrant.core :as ig]))\n(defmethod ig/init-key ::conn [_ o] o)\n",
    )
    .unwrap();
    let cfg = root.join("resources/sys.edn");
    std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();
    std::fs::write(&cfg, "{:sys.db/conn {:url \"x\"}}\n").unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("Indexed");
    client.did_open(&cfg);

    let (line, ch) = position_of(&cfg, ":sys.db/conn");
    let result = client.goto_definition(&cfg, line, ch);
    let uri = result["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("ref-less config should navigate, got {}", result));
    assert!(uri.ends_with("/src/sys/db.clj"), "got {}", uri);
}

#[test]
fn test_e2e_watched_edn_config_keeps_references_fresh() {
    // An Integrant config edited outside the editor (git pull / branch switch)
    // is re-indexed via the file watcher, so references reflect the new keys.
    let project = setup_named("integrant_project");
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("Indexed");

    let db = root.join("src/readx/db.clj");
    let config = root.join("resources/config.edn");
    client.did_open(&db);

    // Baseline: 5 references (3 in db.clj + 2 in config.edn).
    let (line, ch) = position_of(&db, "::db");
    assert_eq!(
        client
            .references(&db, line, ch, true)
            .as_array()
            .unwrap()
            .len(),
        5,
        "baseline references"
    );

    // Rewrite config.edn (no editor save) to drop every :readx.db/db use.
    std::fs::write(&config, "{:other/x {:v 1}\n :sys {:y #ig/ref :other/x}}\n").unwrap();
    client.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": format!("file://{}", config.display()), "type": 2 }] }),
    );

    // Poll until the stale config.edn occurrences are gone: only the 3 db.clj
    // locations remain.
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let n = client
            .references(&db, line, ch, true)
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);
        if n == 3 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "references did not refresh after watched EDN change (still {})",
            n
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn test_e2e_references_spec_keyword_def_and_usage() {
    // find-references on a clojure.spec keyword spans its `s/def` site and every
    // `:req-un`/usage — mirrors tickets/handlers.clj.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let f = root.join("src/handlers.clj");
    std::fs::write(
        &f,
        "(ns app.handlers\n  (:require [clojure.spec.alpha :as s]))\n\
         (s/def :ticket/id integer?)\n\
         (s/def ::ticket-out\n  (s/keys :req-un [:ticket/id]))\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("Indexed");
    client.did_open(&f);

    // From the `s/def` site, and again from the `:req-un` usage — both resolve
    // to the same two locations.
    for needle in [":ticket/id", "[:ticket/id]"] {
        let (line, ch) = position_of(&f, needle);
        let result = client.references(&f, line, ch, true);
        let locs = result
            .as_array()
            .unwrap_or_else(|| panic!("references null from {:?}: {}", needle, result));
        assert_eq!(locs.len(), 2, "from {:?}: {:?}", needle, locs);
    }
}

#[test]
fn test_e2e_zed_client_cross_file_definition() {
    // Under a Zed-shaped init (workspaceFolders, no rootUri), cross-file
    // goto-definition must resolve — the regression that broke Zed navigation.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize_zed(&root);

    let utils = root.join("src/utils.clj");
    client.did_open(&utils);

    let (line, ch) = position_of(&utils, "core/add");
    let def = client.goto_definition(&utils, line, ch);
    let uri = def["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("zed cross-file definition failed: {}", def));
    assert!(uri.ends_with("/src/core.clj"), "got {}", uri);
}

#[test]
fn test_e2e_zed_client_hover_and_completion() {
    // Hover and completion must work under a Zed-shaped client — both need the
    // project (not just the open file) indexed.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize_zed(&root);

    let utils = root.join("src/utils.clj");
    client.did_open(&utils);
    let (line, ch) = position_of(&utils, "core/add");

    // Cross-file docstring on hover.
    let hov = client.hover(&utils, line, ch);
    let val = hov["contents"]["value"]
        .as_str()
        .unwrap_or_else(|| panic!("zed hover returned null: {}", hov));
    assert!(val.contains("Adds two numbers."), "zed hover doc: {}", val);

    // Alias-prefixed completion.
    let comp = client.completion_items(&utils, line, ch);
    let labels: Vec<&str> = comp
        .as_array()
        .unwrap_or_else(|| panic!("zed completion returned null: {}", comp))
        .iter()
        .filter_map(|i| i["label"].as_str())
        .collect();
    assert!(labels.contains(&"core/add"), "zed completion: {:?}", labels);
}

#[test]
fn test_e2e_zed_client_cross_file_references() {
    // find-references across files must work under a Zed-shaped client.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize_zed(&root);

    let core = root.join("src/core.clj");
    client.did_open(&core);

    // `add` is defined in core.clj and used as `core/add` in utils.clj.
    let (line, ch) = position_of(&core, "add");
    let refs = client.references(&core, line, ch, true);
    let locs = refs
        .as_array()
        .unwrap_or_else(|| panic!("zed references returned null: {}", refs));
    assert_eq!(locs.len(), 2, "decl + cross-file usage: {:?}", locs);
    let uris: Vec<&str> = locs.iter().filter_map(|l| l["uri"].as_str()).collect();
    assert!(uris.iter().any(|u| u.ends_with("/src/core.clj")));
    assert!(uris.iter().any(|u| u.ends_with("/src/utils.clj")));
}

/// Prepares a temp copy of the monorepo fixture: the workspace `.gitignore`
/// (excluding `repos/`) and the `.clj-pulse/config.edn` adding the gitignored
/// `repos/b` project are written at runtime — committed into the fixture they
/// would be swallowed by this repo's own gitignore rules.
fn setup_monorepo() -> (tempfile::TempDir, std::path::PathBuf) {
    let project = setup_named("monorepo");
    let root = project.path().canonicalize().unwrap();
    std::fs::write(root.join(".gitignore"), "repos/\n").unwrap();
    std::fs::create_dir_all(root.join(".clj-pulse")).unwrap();
    std::fs::write(
        root.join(".clj-pulse/config.edn"),
        "{:projects [{:path \"repos/b\"}]}\n",
    )
    .unwrap();
    (project, root)
}

/// Multi-project workspace: `clojurePulse/projects` lists the root and every
/// subproject — detected ones plus a gitignored one added via config — with
/// per-project kind, enablement, command, and status.
#[test]
fn test_e2e_monorepo_projects_request() {
    let (_project, root) = setup_monorepo();
    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let result = client.request("clojurePulse/projects", json!(null));
    let list = result.as_array().expect("array of projects");
    let paths: Vec<&str> = list
        .iter()
        .map(|p| p["path"].as_str().expect("path string"))
        .collect();
    assert_eq!(
        paths,
        vec![".", "apps/a", "libs/common", "repos/b"],
        "root first, then rel_path-sorted; repos/b only via the config entry"
    );

    for project in list {
        // The harness kill-switch (CLJ_PULSE_DISABLE_CLASSPATH_CLI) forces
        // every project's classpath resolution off.
        assert_eq!(project["classpath"]["enabled"], json!(false));
        assert_eq!(project["kind"], "deps");
    }
    let a = list.iter().find(|p| p["path"] == "apps/a").unwrap();
    assert_eq!(
        a["classpath"]["cmd"],
        json!("clojure -A:dev:test -Spath"),
        "deps projects carry the default command"
    );
    let status = a["classpath"]["status"].as_str().unwrap();
    assert!(
        ["disabled", "cached", "unresolved"].contains(&status),
        "no stage 3 ran: {status}"
    );
}

/// Live project toggles: `workspace/didChangeConfiguration` enabling a
/// subproject with a stub command drives a stage-3 run (status `resolved`,
/// stubbed library indexed); disabling it again reverts the project to its
/// stage-2 state and drops the stage-3-only library from the union.
#[test]
fn test_e2e_did_change_configuration_toggles_stage3() {
    let (_project, root) = setup_monorepo();
    // Disable the root project's stage 3 in the file config — this test runs
    // without the harness kill-switch, and the root would otherwise spawn a
    // real `clojure`.
    std::fs::write(
        root.join(".clj-pulse/config.edn"),
        "{:projects [{:path \".\" :classpath {:enabled false}}]}\n",
    )
    .unwrap();

    // A fake library dir the stub command reports as the classpath.
    let libdir = tempfile::TempDir::new().unwrap();
    let lib_src = libdir.path().join("stub-lib/src");
    std::fs::create_dir_all(lib_src.join("stub")).unwrap();
    std::fs::write(
        lib_src.join("stub/util.clj"),
        "(ns stub.util)\n\n(defn stubbed [x] x)\n",
    )
    .unwrap();
    let stub = root.join("stub-classpath.sh");
    std::fs::write(&stub, format!("echo '{}'\n", lib_src.display())).unwrap();
    let cmd = format!("sh {}", stub.display());

    let mut client = LspClient::start_with_classpath_cli(&root);
    client.initialize(&root);

    // Enable apps/a with the stub command.
    client.notify(
        "workspace/didChangeConfiguration",
        json!({
            "settings": {
                "clojurePulse": {
                    "projects": [
                        {"path": ".", "classpath": {"enabled": false}},
                        {"path": "apps/a", "classpath": {"enabled": true, "cmd": cmd}}
                    ]
                }
            }
        }),
    );
    client.wait_for_log("full classpath indexed");

    let list = client.request("clojurePulse/projects", json!(null));
    let a = list
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["path"] == "apps/a")
        .expect("apps/a present")
        .clone();
    assert_eq!(a["classpath"]["enabled"], json!(true));
    assert_eq!(a["classpath"]["status"], "resolved");
    let libs = a["libraries"].as_array().unwrap();
    assert!(
        libs.iter()
            .any(|l| l["path"].as_str().unwrap().contains("stub-lib")),
        "stubbed library must appear: {libs:?}"
    );

    // Disable apps/a again: it must revert to its stage-2 state (no .cpcache
    // → unresolved) and the stage-3-only library must drop from the union.
    client.notify(
        "workspace/didChangeConfiguration",
        json!({
            "settings": {
                "clojurePulse": {
                    "projects": [
                        {"path": ".", "classpath": {"enabled": false}},
                        {"path": "apps/a", "classpath": {"enabled": false}}
                    ]
                }
            }
        }),
    );

    // The revert is asynchronous with no distinctive log line; poll.
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let list = client.request("clojurePulse/projects", json!(null));
        let a = list
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["path"] == "apps/a")
            .expect("apps/a present")
            .clone();
        let status = a["classpath"]["status"].as_str().unwrap().to_string();
        let libs = a["libraries"].as_array().unwrap().clone();
        if a["classpath"]["enabled"] == json!(false) && status == "unresolved" && libs.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "apps/a did not revert to stage-2 state: status={status}, libs={libs:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The monorepo headline: goto-definition from `apps/a` into `libs/common`
/// (cross-project, no file from `common` ever opened), and sources of the
/// gitignored `repos/b` — present only via the config entry — indexed and
/// findable through workspace-symbol search.
#[test]
fn test_e2e_monorepo_cross_project_definition() {
    let (_project, root) = setup_monorepo();
    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let consumer = root.join("apps/a/src/a/core.clj");
    client.did_open(&consumer);
    let (line, ch) = position_of(&consumer, "u/helper");
    let result = client.goto_definition(&consumer, line, ch);
    assert!(
        !result.is_null(),
        "cross-project goto-definition returned null"
    );
    let uri = result["uri"].as_str().expect("expected Location");
    assert!(
        uri.ends_with("/libs/common/src/common/util.clj"),
        "definition must land in the common lib: {uri}"
    );
    let def_file = root.join("libs/common/src/common/util.clj");
    let (def_line, _) = position_of(&def_file, "defn helper");
    assert_eq!(result["range"]["start"]["line"], json!(def_line));

    // repos/b is workspace-gitignored; only the explicit config entry makes
    // it a project — and its sources must be indexed despite the gitignore.
    let symbols = client.workspace_symbols("vendored-helper");
    let found = symbols
        .as_array()
        .map(|arr| {
            arr.iter().any(|s| {
                s["location"]["uri"]
                    .as_str()
                    .is_some_and(|u| u.ends_with("/repos/b/src/b/core.clj"))
            })
        })
        .unwrap_or(false);
    assert!(
        found,
        "workspace-symbol must find repos/b's sources: {symbols}"
    );
}

/// A resolved classpath lists the project's own alias dirs (`dev`) alongside
/// real dependencies; the library lists must show only the latter.
#[test]
fn test_e2e_own_dirs_filtered_from_library_lists() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    // Own alias-style dir, present on disk and on the classpath.
    std::fs::create_dir_all(root.join("dev")).unwrap();
    // An out-of-project dependency dir with a real namespace.
    let libdir = tempfile::TempDir::new().unwrap();
    let dep_src = libdir.path().join("dep/src");
    std::fs::create_dir_all(dep_src.join("dep")).unwrap();
    std::fs::write(dep_src.join("dep/core.clj"), "(ns dep.core)\n").unwrap();

    let cpcache = root.join(".cpcache");
    std::fs::create_dir_all(&cpcache).unwrap();
    let sep = if cfg!(windows) { ";" } else { ":" };
    std::fs::write(
        cpcache.join("1.cp"),
        format!("{}{sep}{}", root.join("dev").display(), dep_src.display()),
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("library indexing complete");

    let flat = client.request("clojurePulse/externalLibraries", json!(null));
    let flat_paths: Vec<&str> = flat
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["path"].as_str().unwrap())
        .collect();
    assert!(
        !flat_paths.iter().any(|p| p.ends_with("/dev")),
        "own dir leaked into externalLibraries: {flat_paths:?}"
    );
    assert!(
        flat_paths.iter().any(|p| p.ends_with("/dep/src")),
        "dependency dir missing from externalLibraries: {flat_paths:?}"
    );

    let grouped = client.request("clojurePulse/projects", json!(null));
    let root_libs = grouped
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["path"] == ".")
        .expect("root project present")["libraries"]
        .clone();
    let lib_paths: Vec<&str> = root_libs
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["path"].as_str().unwrap())
        .collect();
    assert!(
        !lib_paths.iter().any(|p| p.ends_with("/dev")),
        "own dir leaked into projects response: {lib_paths:?}"
    );
    assert!(
        lib_paths.iter().any(|p| p.ends_with("/dep/src")),
        "dependency dir missing from projects response: {lib_paths:?}"
    );
}

/// Prepares a project whose root stage-3 command is a stub echoing an
/// existing dir, so classpath resolution runs without a real `clojure`.
fn setup_stub_stage3() -> (tempfile::TempDir, std::path::PathBuf) {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();
    let stub = root.join("stub-classpath.sh");
    std::fs::write(&stub, format!("echo '{}'\n", root.join("src").display())).unwrap();
    std::fs::create_dir_all(root.join(".clj-pulse")).unwrap();
    std::fs::write(
        root.join(".clj-pulse/config.edn"),
        format!(
            "{{:projects [{{:path \".\" :classpath {{:cmd \"sh {}\"}}}}]}}\n",
            stub.display()
        ),
    )
    .unwrap();
    (project, root)
}

/// Stage-3 resolution reports LSP work-done progress when the client
/// advertises `window.workDoneProgress`: a `workDoneProgress/create` request,
/// then `$/progress` Begin and End on a per-run token.
#[test]
fn test_e2e_stage3_reports_work_done_progress() {
    let (_project, root) = setup_stub_stage3();
    let mut client = LspClient::start_with_classpath_cli(&root);
    client.initialize_with_progress(&root);
    client.wait_for_log("full classpath indexed");

    let progress: Vec<Value> = client
        .notifications
        .iter()
        .filter(|m| m["method"] == "$/progress")
        .cloned()
        .collect();
    let token_of = |m: &Value| m["params"]["token"].as_str().unwrap_or("").to_string();
    let begin = progress
        .iter()
        .find(|m| m["params"]["value"]["kind"] == "begin")
        .unwrap_or_else(|| panic!("no $/progress begin: {progress:?}"));
    assert!(
        token_of(begin).starts_with("clj-pulse/classpath/./"),
        "unexpected token: {}",
        token_of(begin)
    );
    assert_eq!(
        begin["params"]["value"]["title"], "Resolving classpath: .",
        "unexpected begin: {begin}"
    );
    assert!(
        progress
            .iter()
            .any(|m| m["params"]["value"]["kind"] == "end" && token_of(m) == token_of(begin)),
        "no matching $/progress end: {progress:?}"
    );
    let create = client
        .notifications
        .iter()
        .any(|m| m["method"] == "window/workDoneProgress/create");
    assert!(create, "workDoneProgress/create request not sent");
}

/// Without the capability, stage 3 must stay silent — no `$/progress`, no
/// `workDoneProgress/create`.
#[test]
fn test_e2e_stage3_no_progress_without_capability() {
    let (_project, root) = setup_stub_stage3();
    let mut client = LspClient::start_with_classpath_cli(&root);
    client.initialize(&root);
    client.wait_for_log("full classpath indexed");

    let noisy = client
        .notifications
        .iter()
        .any(|m| m["method"] == "$/progress" || m["method"] == "window/workDoneProgress/create");
    assert!(!noisy, "progress sent without the capability");
}

impl LspClient {
    /// Counts stashed notifications matching `pred`.
    fn count_notifications(&self, pred: impl Fn(&Value) -> bool) -> usize {
        self.notifications.iter().filter(|m| pred(m)).count()
    }

    /// Waits until more than `prior` stashed notifications match `pred`.
    fn wait_for_notification_beyond(&mut self, prior: usize, pred: impl Fn(&Value) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while self.count_notifications(&pred) <= prior {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_else(|| panic!("timed out waiting for notification"));
            let msg = self
                .incoming
                .recv_timeout(remaining)
                .unwrap_or_else(|_| panic!("timed out waiting for notification"));
            self.stash(msg);
        }
    }
}

/// `clojurePulse/rescan` returns null immediately and always finishes with a
/// `librariesChanged` — even on a fully unchanged workspace with nothing to
/// resolve, so clients get a completion signal.
#[test]
fn test_e2e_rescan_notifies_even_when_nothing_changed() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();
    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let changed = |m: &Value| m["method"] == "clojurePulse/librariesChanged";
    let prior = client.count_notifications(changed);
    let result = client.request("clojurePulse/rescan", json!(null));
    assert!(result.is_null(), "rescan must return null: {result}");
    client.wait_for_notification_beyond(prior, changed);
}

/// A second rescan re-runs the classpath command for an already-resolved
/// project, with a fresh progress token per run.
#[test]
fn test_e2e_rescan_reruns_resolved_project() {
    let (_project, root) = setup_stub_stage3();
    let mut client = LspClient::start_with_classpath_cli(&root);
    client.initialize_with_progress(&root);
    client.wait_for_log("full classpath indexed");

    let begin = |m: &Value| m["method"] == "$/progress" && m["params"]["value"]["kind"] == "begin";
    let prior = client.count_notifications(begin);
    assert!(prior >= 1, "startup resolution must have reported progress");
    client.request("clojurePulse/rescan", json!(null));
    client.wait_for_notification_beyond(prior, begin);

    let tokens: std::collections::HashSet<String> = client
        .notifications
        .iter()
        .filter(|m| begin(m))
        .map(|m| m["params"]["token"].as_str().unwrap().to_string())
        .collect();
    assert!(
        tokens.len() > prior,
        "each run needs its own token: {tokens:?}"
    );
}

/// The target scenario: a gitignored subproject created *after* initialize
/// (ignored dirs fire no watchers) appears in `clojurePulse/projects` after a
/// rescan, because the config lists it.
#[test]
fn test_e2e_rescan_picks_up_new_gitignored_subproject() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();
    std::fs::write(root.join(".gitignore"), "vend/\n").unwrap();
    // Listed up front; the dir does not exist yet, so the entry is ignored
    // with a warning at startup.
    std::fs::create_dir_all(root.join(".clj-pulse")).unwrap();
    std::fs::write(
        root.join(".clj-pulse/config.edn"),
        "{:projects [{:path \"vend/x\"}]}\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    let list = client.request("clojurePulse/projects", json!(null));
    assert!(
        !list
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["path"] == "vend/x"),
        "vend/x must not exist before creation: {list}"
    );

    // Created after initialize; gitignored, so no watcher will ever fire.
    std::fs::create_dir_all(root.join("vend/x/src/vx")).unwrap();
    std::fs::write(root.join("vend/x/deps.edn"), "{:paths [\"src\"]}\n").unwrap();
    std::fs::write(root.join("vend/x/src/vx/core.clj"), "(ns vx.core)\n").unwrap();

    client.request("clojurePulse/rescan", json!(null));

    // Startup's own librariesChanged can still be in flight, so poll the
    // projects request instead of counting notifications.
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let list = client.request("clojurePulse/projects", json!(null));
        if list
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["path"] == "vend/x")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "rescan must pick up the new gitignored subproject: {list}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

// --- clj-kondo diagnostics bridge -------------------------------------------
//
// These drive the committed fake `clj-kondo` (tests/fixtures/fake-clj-kondo),
// put on the server's PATH by `start_with_kondo`. The `kondo_project` fixture's
// source files each carry a marker the fake recognises, and its findings are
// pinned to the `helpers/greet` call on line 4 of each file.

/// The kondo fixture, plus the `.clj-kondo` directory clj-kondo requires
/// before it will read a config or write a cache. It is created here rather
/// than committed because the repo's `.gitignore` excludes `.clj-kondo`.
fn setup_kondo_project() -> tempfile::TempDir {
    let tmp = setup_named("kondo_project");
    std::fs::create_dir_all(tmp.path().join(".clj-kondo")).unwrap();
    std::fs::write(tmp.path().join(".clj-kondo/config.edn"), "{}\n").unwrap();
    tmp
}

/// The diagnostics of the first publish for `uri_suffix`, as (code, source)
/// pairs — what these tests actually assert on.
fn diagnostic_codes(params: &Value) -> Vec<(String, String)> {
    params["diagnostics"]
        .as_array()
        .expect("diagnostics array")
        .iter()
        .map(|d| {
            (
                d["code"].as_str().unwrap_or_default().to_string(),
                d["source"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

#[test]
fn test_e2e_unused_binding_diagnostic() {
    // With clj-kondo off (the harness default), the native unused-binding lint
    // is what the editor sees.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let scratch = root.join("src/scratch.clj");
    std::fs::write(
        &scratch,
        "(ns simple.scratch)\n\n(defn run [x y]\n  (let [z 1]\n    x))\n",
    )
    .unwrap();
    client.did_open(&scratch);

    let params = client.wait_for_diagnostics("/src/scratch.clj");
    let found: Vec<&Value> = params["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["code"] == json!("unused-binding"))
        .collect();
    assert_eq!(found.len(), 2, "the param `y` and the let `z`: {}", params);
    for d in &found {
        assert_eq!(d["severity"], json!(2), "WARNING: {}", d);
        assert_eq!(d["tags"], json!([1]), "UNNECESSARY: {}", d);
        assert_eq!(d["source"], json!("clj-pulse"));
    }
    // Inner scopes are reported first (frames are harvested as they pop), so
    // assert on the set rather than the order.
    let mut messages: Vec<&str> = found
        .iter()
        .map(|d| d["message"].as_str().unwrap())
        .collect();
    messages.sort_unstable();
    assert_eq!(
        messages,
        vec!["Unused binding: y", "Unused binding: z"],
        "{}",
        params
    );
}

#[test]
fn test_e2e_unused_private_var_diagnostic() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let scratch = root.join("src/scratch.clj");
    std::fs::write(
        &scratch,
        "(ns simple.scratch)\n\n(defn- helper [] 1)\n\n(defn run [] 2)\n",
    )
    .unwrap();
    client.did_open(&scratch);

    let params = client.wait_for_diagnostics("/src/scratch.clj");
    let found: Vec<&Value> = params["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["code"] == json!("unused-private-var"))
        .collect();
    assert_eq!(found.len(), 1, "only `helper` is dead: {}", params);
    assert_eq!(found[0]["severity"], json!(2));
    assert_eq!(found[0]["tags"], json!([1]));
    assert!(
        found[0]["message"].as_str().unwrap().contains("helper"),
        "{}",
        found[0]
    );
}

/// A buffer over 1 KiB carrying the fake kondo's error marker, so both tiers
/// have something to say and the `:live-max-kb 1` threshold applies.
fn write_large_kondo_file(root: &Path) -> std::path::PathBuf {
    let big = root.join("src/big.clj");
    let mut source = String::from(
        "(ns kondo.big)\n;; kondo-finding-here\n(defn run []\n  (helpers/greet \"world\"))\n",
    );
    while source.len() <= 1024 {
        source.push_str(";; padding so the buffer is larger than one kibibyte\n");
    }
    std::fs::write(&big, source).unwrap();
    big
}

#[test]
fn test_e2e_kondo_threshold_skips_keystrokes_on_large_buffers() {
    // Above `:live-max-kb`, clj-kondo sits out the didChange pass — the native
    // set publishes alone — but still runs on open and on save. The engine
    // itself stays active: no lint-status change, no re-probe.
    let project = setup_kondo_project();
    let root = project.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".clj-pulse")).unwrap();
    std::fs::write(
        root.join(".clj-pulse/config.edn"),
        "{:kondo {:live-max-kb 1}}\n",
    )
    .unwrap();

    let mut client = LspClient::start_with_kondo(&root);
    client.initialize(&root);
    client.wait_for_log("clj-kondo v0.0.0-fake found");

    let big = write_large_kondo_file(&root);
    client.did_open(&big);
    let params = client.wait_for_diagnostics("/src/big.clj");
    assert_eq!(
        diagnostic_codes(&params),
        vec![("unresolved-symbol".to_string(), "clj-kondo".to_string())],
        "didOpen always runs clj-kondo"
    );

    client.clear_notifications();
    client.did_change_insert(&big, 1, 0, ";; typing\n");
    let params = client.wait_for_diagnostics("/src/big.clj");
    assert_eq!(
        diagnostic_codes(&params),
        vec![("unresolved-namespace".to_string(), "clj-pulse".to_string())],
        "a keystroke on a buffer above the threshold publishes native lints only"
    );

    client.clear_notifications();
    client.notify(
        "textDocument/didSave",
        json!({ "textDocument": { "uri": format!("file://{}", big.display()) } }),
    );
    let params = client.wait_for_diagnostics("/src/big.clj");
    assert_eq!(
        diagnostic_codes(&params),
        vec![("unresolved-symbol".to_string(), "clj-kondo".to_string())],
        "didSave always runs clj-kondo"
    );
}

#[test]
fn test_e2e_kondo_threshold_save_right_after_change_keeps_kondo_findings() {
    // Edit, then save inside the debounce window. The save runs clj-kondo and
    // publishes; the change pass still waiting on the edit must then stand
    // down, or its native-only set would erase the findings the save produced
    // — same version, no further edit, nothing to bring them back until the
    // next save.
    let project = setup_kondo_project();
    let root = project.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".clj-pulse")).unwrap();
    std::fs::write(
        root.join(".clj-pulse/config.edn"),
        "{:kondo {:live-max-kb 1}}\n",
    )
    .unwrap();

    let mut client = LspClient::start_with_kondo(&root);
    client.initialize(&root);
    client.wait_for_log("clj-kondo v0.0.0-fake found");

    let big = write_large_kondo_file(&root);
    client.did_open(&big);
    client.wait_for_diagnostics("/src/big.clj");

    client.clear_notifications();
    client.did_change_insert(&big, 1, 0, ";; typing\n");
    client.notify(
        "textDocument/didSave",
        json!({ "textDocument": { "uri": format!("file://{}", big.display()) } }),
    );
    let params = client.wait_for_diagnostics("/src/big.clj");
    assert_eq!(
        diagnostic_codes(&params),
        vec![("unresolved-symbol".to_string(), "clj-kondo".to_string())],
        "the save's full pass publishes first"
    );

    // Outlast the debounce, then make sure nothing native-only followed.
    std::thread::sleep(Duration::from_millis(3 * 300));
    while let Ok(msg) = client.incoming.try_recv() {
        if msg["method"] == "textDocument/publishDiagnostics"
            && msg["params"]["uri"]
                .as_str()
                .is_some_and(|u| u.ends_with("/src/big.clj"))
        {
            assert_eq!(
                diagnostic_codes(&msg["params"]),
                vec![("unresolved-symbol".to_string(), "clj-kondo".to_string())],
                "a pending change pass overwrote the save's findings: {}",
                msg["params"]
            );
        }
        client.stash(msg);
    }
}

#[test]
fn test_e2e_kondo_threshold_zero_means_no_limit() {
    let project = setup_kondo_project();
    let root = project.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".clj-pulse")).unwrap();
    std::fs::write(
        root.join(".clj-pulse/config.edn"),
        "{:kondo {:live-max-kb 0}}\n",
    )
    .unwrap();

    let mut client = LspClient::start_with_kondo(&root);
    client.initialize(&root);
    client.wait_for_log("clj-kondo v0.0.0-fake found");

    let big = write_large_kondo_file(&root);
    client.did_open(&big);
    client.wait_for_diagnostics("/src/big.clj");

    client.clear_notifications();
    client.did_change_insert(&big, 1, 0, ";; typing\n");
    let params = client.wait_for_diagnostics("/src/big.clj");
    assert_eq!(
        diagnostic_codes(&params),
        vec![("unresolved-symbol".to_string(), "clj-kondo".to_string())],
        "with no limit, a keystroke runs clj-kondo too"
    );
}

/// A directory holding a copy of the fake clj-kondo, standing in for a mise
/// shims or Homebrew bin directory the editor's PATH does not list.
fn well_known_dir_with_fake_kondo() -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().unwrap();
    let target = dir.path().join("clj-kondo");
    std::fs::copy(LspClient::fake_kondo_dir().join("clj-kondo"), &target).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

/// A PATH holding only the system directories the fake needs (`sh`, `cat`),
/// and no clj-kondo — what a Dock-launched editor hands the server.
const BARE_PATH: &str = "/usr/bin:/bin";

#[test]
fn test_e2e_kondo_found_in_a_well_known_dir_off_path() {
    // Homebrew and mise install directories are searched after PATH, so a
    // clj-kondo the editor's PATH does not list is still found — and the
    // announcement names the file that ran.
    let project = setup_kondo_project();
    let root = project.path().canonicalize().unwrap();
    let shims = well_known_dir_with_fake_kondo();

    let mut client = LspClient::spawn(
        &root,
        &[
            ("CLJ_PULSE_TOOL_DIRS", shims.path()),
            ("PATH", Path::new(BARE_PATH)),
        ],
        true,
        Kondo::Real,
    );
    client.initialize(&root);
    client.wait_for_log(&format!(
        "clj-kondo v0.0.0-fake found ({})",
        shims.path().join("clj-kondo").display()
    ));

    let app = root.join("src/app.clj");
    client.did_open(&app);
    let params = client.wait_for_diagnostics("/src/app.clj");
    assert_eq!(
        diagnostic_codes(&params),
        vec![("unresolved-symbol".to_string(), "clj-kondo".to_string())]
    );
}

#[test]
fn test_e2e_kondo_not_found_says_where_it_looked() {
    let project = setup_kondo_project();
    let root = project.path().canonicalize().unwrap();
    let empty = tempfile::TempDir::new().unwrap();

    let mut client = LspClient::spawn(
        &root,
        &[
            ("CLJ_PULSE_TOOL_DIRS", empty.path()),
            ("PATH", Path::new(BARE_PATH)),
        ],
        true,
        Kondo::Real,
    );
    client.initialize(&root);
    client.wait_for_log("clj-kondo not found — linting: native lints only");
    let line = client
        .notifications
        .iter()
        .filter(|m| m["method"] == "window/logMessage")
        .map(|m| {
            m["params"]["message"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .find(|m| m.contains("clj-kondo not found"))
        .unwrap();
    assert!(line.contains("PATH (2 entries)"), "{line}");
    assert!(
        line.contains(&empty.path().display().to_string()),
        "names the well-known dirs it tried: {line}"
    );
    assert!(line.contains("clojurePulse.kondo.path"), "{line}");

    // The same reason rides the lint status, for the editor to show. An
    // earlier status without it is legitimate: `initialized` sends the
    // current state, which may predate the probe.
    let status = client.wait_for_notification_where("clojurePulse/lintStatus", |p| {
        p["detail"]
            .as_str()
            .is_some_and(|d| d.contains("clj-kondo not found"))
    });
    assert_eq!(status["engine"], json!("native"));
}

#[test]
fn test_e2e_kondo_workspace_relative_path_lints_files_in_subdirectories() {
    // `:path "./bin/clj-kondo"` is anchored to the workspace. A lint runs from
    // the file's own directory (so mise shims see the project's pin), which
    // must not turn that path into `src/bin/clj-kondo`.
    let project = setup_kondo_project();
    let root = project.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("bin")).unwrap();
    let target = root.join("bin/clj-kondo");
    std::fs::copy(LspClient::fake_kondo_dir().join("clj-kondo"), &target).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::create_dir_all(root.join(".clj-pulse")).unwrap();
    std::fs::write(
        root.join(".clj-pulse/config.edn"),
        "{:kondo {:path \"./bin/clj-kondo\"}}\n",
    )
    .unwrap();

    let mut client = LspClient::spawn(&root, &[("PATH", Path::new(BARE_PATH))], true, Kondo::Real);
    client.initialize(&root);
    client.wait_for_log(&format!(
        "clj-kondo v0.0.0-fake found ({})",
        root.join("./bin/clj-kondo").display()
    ));

    let app = root.join("src/app.clj");
    client.did_open(&app);
    let params = client.wait_for_diagnostics("/src/app.clj");
    assert_eq!(
        diagnostic_codes(&params),
        vec![("unresolved-symbol".to_string(), "clj-kondo".to_string())],
        "the lint from src/ must still run the workspace's bin/clj-kondo"
    );
}

#[test]
fn test_e2e_kondo_path_that_is_a_command_line_is_explained() {
    // `mise exec -- clj-kondo` is a natural thing to type into a "path"
    // setting; it is spawned as one program name and cannot work. Say so.
    let project = setup_kondo_project();
    let root = project.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".clj-pulse")).unwrap();
    std::fs::write(
        root.join(".clj-pulse/config.edn"),
        "{:kondo {:path \"mise exec -- clj-kondo\"}}\n",
    )
    .unwrap();

    let mut client = LspClient::start_with_kondo(&root);
    client.initialize(&root);
    client.wait_for_log("names a program, not a command line");
}

#[test]
fn test_e2e_kondo_run_drops_native_unused_binding() {
    // A successful clj-kondo run owns `unused-binding` too: the fake returns
    // no findings with exit 0, so the native copy must not survive.
    let project = setup_kondo_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start_with_kondo(&root);
    client.initialize(&root);
    client.wait_for_log("clj-kondo v0.0.0-fake found");

    // No fake-kondo marker in this file, so the fake reports nothing — but it
    // still succeeds, which is what cedes ownership.
    let scratch = root.join("src/scratch.clj");
    std::fs::write(
        &scratch,
        "(ns kondo.scratch)\n\n(defn run [x]\n  (let [z 1]\n    x))\n",
    )
    .unwrap();
    client.did_open(&scratch);

    let params = client.wait_for_diagnostics("/src/scratch.clj");
    assert!(
        params["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .all(|d| d["code"] != json!("unused-binding")),
        "clj-kondo owns unused-binding: {}",
        params
    );
}

#[test]
fn test_e2e_kondo_findings_published_and_native_codes_ceded() {
    let project = setup_kondo_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start_with_kondo(&root);
    client.initialize(&root);
    // The probe runs off the initialize critical path, so wait for it — a
    // didOpen that beats it would legitimately publish native-only.
    client.wait_for_log("clj-kondo v0.0.0-fake found");

    // app.clj carries the marker AND an unresolved `helpers/greet` usage, so
    // both engines have something to say about it.
    let app = root.join("src/app.clj");
    client.did_open(&app);

    let params = client.wait_for_diagnostics("/src/app.clj");
    assert_eq!(
        diagnostic_codes(&params),
        vec![("unresolved-symbol".to_string(), "clj-kondo".to_string())],
        "clj-kondo owns unresolved-namespace, so the native one must not \
         appear beside its finding"
    );

    // The finding maps to the `helpers/greet` call: line 4 (0-based 3),
    // characters 3..16.
    let range = &params["diagnostics"][0]["range"];
    assert_eq!(range["start"], json!({ "line": 3, "character": 3 }));
    assert_eq!(range["end"], json!({ "line": 3, "character": 16 }));
    assert_eq!(params["diagnostics"][0]["severity"], json!(1)); // ERROR
}

#[test]
fn test_e2e_without_kondo_publishes_native_lints_only() {
    let project = setup_kondo_project();
    let root = project.path().canonicalize().unwrap();

    // Plain `start`: the harness kill-switch is on, exactly as for every other
    // test in this file — behavior must be what it was before the bridge.
    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("linting: native lints only");

    let app = root.join("src/app.clj");
    client.did_open(&app);

    let params = client.wait_for_diagnostics("/src/app.clj");
    assert_eq!(
        diagnostic_codes(&params),
        vec![("unresolved-namespace".to_string(), "clj-pulse".to_string())]
    );
}

#[test]
fn test_e2e_kondo_disabled_by_config_publishes_native_only() {
    let project = setup_kondo_project();
    let root = project.path().canonicalize().unwrap();

    // The fake is on PATH, but the project turns clj-kondo off — it must never
    // be probed or spawned.
    std::fs::create_dir_all(root.join(".clj-pulse")).unwrap();
    std::fs::write(
        root.join(".clj-pulse/config.edn"),
        "{:kondo {:enabled false}}\n",
    )
    .unwrap();

    let mut client = LspClient::start_with_kondo(&root);
    client.initialize(&root);
    client.wait_for_log("clj-kondo disabled — linting: native lints only");

    let app = root.join("src/app.clj");
    client.did_open(&app);

    let params = client.wait_for_diagnostics("/src/app.clj");
    assert_eq!(
        diagnostic_codes(&params),
        vec![("unresolved-namespace".to_string(), "clj-pulse".to_string())]
    );
}

#[test]
fn test_e2e_add_require_action_binds_to_a_kondo_diagnostic() {
    let project = setup_kondo_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start_with_kondo(&root);
    client.initialize(&root);
    client.wait_for_log("clj-kondo v0.0.0-fake found");

    // consumer.clj's marker makes the fake emit `unresolved-namespace` — the
    // code the add-require lightbulb keys on. It matches by code, not source,
    // so the fix must still be offered for a clj-kondo-sourced diagnostic.
    let consumer = root.join("src/consumer.clj");
    client.did_open(&consumer);

    let params = client.wait_for_diagnostics("/src/consumer.clj");
    let diags = params["diagnostics"].as_array().expect("diagnostics array");
    let unresolved = diags
        .iter()
        .find(|d| d["source"] == json!("clj-kondo"))
        .expect("expected a clj-kondo diagnostic");
    assert_eq!(unresolved["code"], json!("unresolved-namespace"));

    let result = client.code_action_for_diagnostic(&consumer, unresolved);
    let actions = result.as_array().expect("code action array");
    let action = actions
        .iter()
        .find(|a| {
            a["title"]
                .as_str()
                .map(|t| t.contains("[kondo.helpers :as helpers]"))
                .unwrap_or(false)
        })
        .expect("expected the add-require action for a clj-kondo diagnostic");
    assert_eq!(action["kind"], json!("quickfix"));
    assert_eq!(action["diagnostics"][0]["source"], json!("clj-kondo"));
}

#[test]
fn test_e2e_disabling_kondo_live_relints_open_documents() {
    let project = setup_kondo_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start_with_kondo(&root);
    client.initialize(&root);
    client.wait_for_log("clj-kondo v0.0.0-fake found");

    let app = root.join("src/app.clj");
    client.did_open(&app);
    let params = client.wait_for_diagnostics("/src/app.clj");
    assert_eq!(params["diagnostics"][0]["source"], json!("clj-kondo"));

    // Turning clj-kondo off must take effect without an edit or a restart:
    // the toggle re-probes and re-lints every open buffer.
    client.clear_notifications();
    let config = root.join(".clj-pulse/config.edn");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(&config, "{:kondo {:enabled false}}\n").unwrap();
    client.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": format!("file://{}", config.display()), "type": 1 }] }),
    );
    client.wait_for_log("clj-kondo disabled");

    let params = client.wait_for_diagnostics("/src/app.clj");
    assert_eq!(
        diagnostic_codes(&params),
        vec![("unresolved-namespace".to_string(), "clj-pulse".to_string())],
        "the native set must come back once clj-kondo is switched off"
    );
}

#[test]
fn test_e2e_kondo_cache_warmed_from_the_resolved_classpath() {
    let project = setup_kondo_project();
    let root = project.path().canonicalize().unwrap();

    // A stage-2 classpath: a `.cpcache` naming one existing absolute entry
    // (an absolute path is what `discover` treats as a live cache).
    let lib = root.join("libs/some-lib");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::create_dir_all(root.join(".cpcache")).unwrap();
    std::fs::write(
        root.join(".cpcache/fixture.cp"),
        format!("src:{}", lib.display()),
    )
    .unwrap();

    // The fake records every `--dependencies` invocation here.
    let log = root.join("kondo-warm.log");
    let mut client = LspClient::start_with_kondo_env(&root, &[("FAKE_KONDO_LOG", log.as_path())]);
    client.initialize(&root);

    client.wait_for_log("clj-kondo cache warm complete");

    let recorded = std::fs::read_to_string(&log).expect("the fake should have logged a warm run");
    assert!(
        recorded.contains("--dependencies"),
        "warming must populate the cache without reporting findings: {recorded}"
    );
    assert!(
        recorded.contains("--parallel"),
        "warming should scan in parallel: {recorded}"
    );
    assert!(
        recorded.contains(&lib.display().to_string()),
        "the resolved classpath must be what gets scanned: {recorded}"
    );
}

#[test]
fn test_e2e_kondo_cache_not_warmed_without_a_clj_kondo_dir() {
    // clj-kondo never creates `.clj-kondo`, and without it a dependency scan
    // runs for minutes and persists nothing — so it must not run at all.
    let project = setup_named("kondo_project");
    let root = project.path().canonicalize().unwrap();

    let lib = root.join("libs/some-lib");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::create_dir_all(root.join(".cpcache")).unwrap();
    std::fs::write(
        root.join(".cpcache/fixture.cp"),
        format!("src:{}", lib.display()),
    )
    .unwrap();

    let log = root.join("kondo-warm.log");
    let mut client = LspClient::start_with_kondo_env(&root, &[("FAKE_KONDO_LOG", log.as_path())]);
    client.initialize(&root);
    // Sync on something that necessarily follows the indexing task, then
    // assert the warm never happened.
    client.wait_for_log("clj-kondo v0.0.0-fake found");
    client.wait_for_log("library indexing complete");

    // A buffer lint still works — only warming is gated on the directory.
    let app = root.join("src/app.clj");
    client.did_open(&app);
    let params = client.wait_for_diagnostics("/src/app.clj");
    assert_eq!(params["diagnostics"][0]["source"], json!("clj-kondo"));

    assert!(
        !log.exists(),
        "no .clj-kondo dir means no dependency scan, got: {}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
}

/// Whether the host has a usable `clj-kondo` for the real-binary smoke test.
fn real_clj_kondo_available() -> bool {
    std::process::Command::new("clj-kondo")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// The bridge end to end against a real clj-kondo, not the fake: proves the
/// argv, the stdin feed, the exit codes, and the JSON shape still match what a
/// released clj-kondo actually does. Requires the binary, so it is ignored by
/// default: `bb e2e-real-kondo`.
#[test]
#[ignore = "requires a real clj-kondo binary on PATH"]
fn test_e2e_real_kondo_publishes_findings() {
    if !real_clj_kondo_available() {
        eprintln!("SKIP: no clj-kondo on PATH — install it to run this test");
        return;
    }

    let project = setup_kondo_project();
    let root = project.path().canonicalize().unwrap();

    // An unresolved *symbol*: no native lint reports this, so seeing it proves
    // the finding came from clj-kondo and not from clj-pulse's own analysis.
    let smoke = root.join("src/smoke.clj");
    std::fs::write(
        &smoke,
        "(ns smoke)\n\n(defn run []\n  (definitely-not-defined 1))\n",
    )
    .unwrap();

    let mut client = LspClient::start_with_real_kondo(&root);
    client.initialize(&root);
    client.wait_for_log("linting: clj-kondo + native");
    client.did_open(&smoke);

    let params = client.wait_for_diagnostics("/src/smoke.clj");
    let diags = params["diagnostics"].as_array().expect("diagnostics array");
    let finding = diags
        .iter()
        .find(|d| d["code"] == json!("unresolved-symbol"))
        .unwrap_or_else(|| panic!("no unresolved-symbol from clj-kondo: {params}"));
    assert_eq!(finding["source"], json!("clj-kondo"));
    assert!(
        finding["message"]
            .as_str()
            .unwrap_or_default()
            .contains("definitely-not-defined"),
        "unexpected message: {finding}"
    );
}

/// `initializationOptions` pointing the server at the committed fixture
/// export (`tests/fixtures/clojuredocs/export.json`, official shape).
fn clojuredocs_options() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/clojuredocs/export.json");
    json!({ "clojuredocs": { "path": path.to_str().unwrap() } })
}

fn clojure_docs_at(client: &mut LspClient, file: &Path, needle: &str) -> Value {
    let (line, ch) = position_of(file, needle);
    client.clojure_docs(json!({
        "textDocument": { "uri": format!("file://{}", file.display()) },
        "position": { "line": line, "character": ch }
    }))
}

#[test]
fn test_e2e_clojuredocs_bare_core_symbol() {
    // `map` under the cursor: the clojure.core entry, examples and see-alsos
    // included, notes and contributor metadata stripped.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();
    let f = root.join("src/docs_demo.clj");
    std::fs::write(&f, "(ns simple.docs-demo)\n(map inc [1 2 3])\n").unwrap();
    let mut client = LspClient::start(&root);
    client.initialize_with_options(&root, clojuredocs_options());
    client.did_open(&f);

    let msg = clojure_docs_at(&mut client, &f, "map ");
    assert!(msg.get("error").is_none(), "unexpected error: {msg}");
    let result = &msg["result"];
    assert_eq!(result["symbol"], "clojure.core/map", "{result}");
    let entry = &result["entry"];
    assert_eq!(entry["ns"], "clojure.core");
    assert_eq!(entry["name"], "map");
    assert_eq!(entry["added"], "1.0");
    assert_eq!(entry["arglists"], json!(["[f]", "[f coll]", "[f c1 c2]"]));
    assert_eq!(
        entry["examples"].as_array().map(Vec::len),
        Some(2),
        "{entry}"
    );
    assert_eq!(entry["examples"][0], "(map inc [1 2 3])\n;;=> (2 3 4)");
    assert_eq!(
        entry["seeAlsos"],
        json!(["clojure.core/mapv", "clojure.core/pmap"])
    );
    assert_eq!(entry["url"], "https://clojuredocs.org/clojure.core/map");
    assert!(
        entry.get("notes").is_none(),
        "notes must not be served: {entry}"
    );
    assert!(
        !msg.to_string().contains("avatar"),
        "contributor metadata leaked: {msg}"
    );
}

#[test]
fn test_e2e_clojuredocs_aliased_symbol() {
    // `str/join` resolves through the ns form's alias even though nothing
    // from clojure.string is indexed (no jar on this fixture's classpath).
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();
    let f = root.join("src/docs_alias.clj");
    std::fs::write(
        &f,
        "(ns simple.docs-alias (:require [clojure.string :as str]))\n(str/join \",\" [1 2])\n",
    )
    .unwrap();
    let mut client = LspClient::start(&root);
    client.initialize_with_options(&root, clojuredocs_options());
    client.did_open(&f);

    let msg = clojure_docs_at(&mut client, &f, "str/join");
    assert!(msg.get("error").is_none(), "unexpected error: {msg}");
    let result = &msg["result"];
    assert_eq!(result["symbol"], "clojure.string/join", "{result}");
    assert_eq!(
        result["entry"]["examples"].as_array().map(Vec::len),
        Some(1)
    );
    assert_eq!(
        result["entry"]["arglists"],
        json!(["[coll]", "[separator coll]"])
    );
}

#[test]
fn test_e2e_clojuredocs_direct_symbol_lookup() {
    // See-also links look a var up by name; a var ClojureDocs has no entry for
    // still echoes the symbol so the editor can say what it looked for.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();
    let mut client = LspClient::start(&root);
    client.initialize_with_options(&root, clojuredocs_options());

    let msg = client.clojure_docs(json!({ "symbol": "clojure.core/pmap" }));
    assert!(msg.get("error").is_none(), "unexpected error: {msg}");
    assert_eq!(msg["result"]["symbol"], "clojure.core/pmap");
    assert!(msg["result"]["entry"].is_null(), "{msg}");

    let msg = client.clojure_docs(json!({ "symbol": "clojure.string/join" }));
    assert_eq!(msg["result"]["entry"]["name"], "join", "{msg}");
}

#[test]
fn test_e2e_clojuredocs_not_configured() {
    // Without `initializationOptions.clojuredocs.path` the request errors
    // with a message the editor can show, rather than answering "no entry".
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();
    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let msg = client.clojure_docs(json!({ "symbol": "clojure.core/map" }));
    let err = msg
        .get("error")
        .unwrap_or_else(|| panic!("expected an error, got {msg}"));
    assert!(
        err["message"]
            .as_str()
            .unwrap_or("")
            .contains("not configured"),
        "{err}"
    );
}

#[test]
fn test_e2e_clojuredocs_unreadable_file() {
    // A configured path that cannot be read errors with a message naming
    // the problem, and keeps answering the same way without re-reading.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();
    let mut client = LspClient::start(&root);
    let missing = root.join("no-such-clojuredocs.json");
    client.initialize_with_options(
        &root,
        json!({ "clojuredocs": { "path": missing.to_str().unwrap() } }),
    );

    for _ in 0..2 {
        let msg = client.clojure_docs(json!({ "symbol": "clojure.core/map" }));
        let err = msg
            .get("error")
            .unwrap_or_else(|| panic!("expected an error, got {msg}"));
        assert!(
            err["message"]
                .as_str()
                .unwrap_or("")
                .contains("could not be loaded"),
            "{err}"
        );
    }
}

#[test]
fn test_e2e_definition_reaches_declare_site() {
    // `only-declared` is never defined; its `(declare …)` is the definition.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let file = root.join("src/ns_options.clj");
    client.did_open(&file);
    let text = std::fs::read_to_string(&file).unwrap();

    let (line, ch) = start_of(&text, "(only-declared x)");
    let result = client.goto_definition(&file, line, ch + 3);
    let uri = result["uri"].as_str().expect("expected Location");
    assert!(uri.ends_with("/src/ns_options.clj"), "got {}", uri);

    let (decl_line, _) = start_of(&text, "(declare only-declared)");
    assert_eq!(result["range"]["start"]["line"], json!(decl_line));
}

#[test]
fn test_e2e_declare_defers_to_the_real_definition() {
    // `defined-later` is declared and then defined: definition lands on the
    // `defn`, while references and rename still reach the declare site.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let file = root.join("src/ns_options.clj");
    client.did_open(&file);
    let text = std::fs::read_to_string(&file).unwrap();

    let (use_line, use_ch) = start_of(&text, "(defined-later m)");
    let result = client.goto_definition(&file, use_line, use_ch + 3);
    let (defn_line, _) = start_of(&text, "(defn defined-later [x]");
    assert_eq!(
        result["range"]["start"]["line"],
        json!(defn_line),
        "definition should be the defn, not the declare: {}",
        result
    );

    let (decl_line, decl_ch) = start_of(&text, "(declare defined-later)");
    let refs = client.references(&file, use_line, use_ch + 3, true);
    let lines: Vec<u64> = refs
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["range"]["start"]["line"].as_u64().unwrap())
        .collect();
    assert!(
        lines.contains(&(decl_line as u64)),
        "declare site missing from references: {:?}",
        lines
    );

    let result = client.rename(&file, decl_line, decl_ch + 12, "later");
    let changes = result["changes"].as_object().unwrap();
    let edits = changes.values().next().unwrap().as_array().unwrap();
    assert!(
        edits
            .iter()
            .any(|e| e["range"]["start"]["line"] == json!(decl_line)),
        "declare site not renamed: {:?}",
        edits
    );
}

#[test]
fn test_e2e_as_alias_keyword_navigates_and_completes() {
    // `[simple.config :as-alias cfg]` binds `cfg` without requiring the
    // namespace: `::cfg/port` resolves to its Integrant key, and the alias is
    // offered in completion.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let file = root.join("src/ns_options.clj");
    let config = root.join("src/config.clj");
    client.did_open(&file);
    let text = std::fs::read_to_string(&file).unwrap();

    let (line, ch) = start_of(&text, "::cfg/port");
    let result = client.goto_definition(&file, line, ch + 3);
    let uri = result["uri"].as_str().expect("expected Location");
    assert!(uri.ends_with("/src/config.clj"), "got {}", uri);
    let (key_line, _) = start_of(
        &std::fs::read_to_string(&config).unwrap(),
        "(defmethod ig/init-key ::port",
    );
    assert_eq!(result["range"]["start"]["line"], json!(key_line));

    // Completion offers the alias itself.
    let last_line = text.lines().count() as u32;
    client.did_change_insert(&file, last_line, 0, "cf");
    let items = client.completion_items(&file, last_line, 2);
    let items = items["items"].as_array().unwrap_or_else(|| {
        items
            .as_array()
            .unwrap_or_else(|| panic!("unexpected completion shape: {}", items))
    });
    let cfg = items
        .iter()
        .find(|i| i["label"] == json!("cfg"))
        .unwrap_or_else(|| panic!("cfg alias not offered: {}", json!(items)));
    assert_eq!(cfg["detail"], json!("alias for simple.config"));
}

#[test]
fn test_e2e_refer_clojure_rename_hovers_core_doc() {
    // `(:refer-clojure :rename {map cmap})` — `cmap` is `clojure.core/map`,
    // so hover shows the curated core entry for `map`.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let file = root.join("src/ns_options.clj");
    client.did_open(&file);
    let text = std::fs::read_to_string(&file).unwrap();

    let (line, ch) = start_of(&text, "(cmap inc");
    let hover = client.hover(&file, line, ch + 2);
    let value = hover["contents"]["value"]
        .as_str()
        .unwrap_or_else(|| panic!("no hover for cmap: {}", hover));
    assert!(
        value.contains("map"),
        "hover does not describe core map: {}",
        value
    );
}

#[test]
fn test_e2e_definition_through_prefix_list_alias() {
    // `(simple [helpers :as h])` — the prefix list binds `h` to
    // `simple.helpers`, so `h/greet` navigates to helpers.clj.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let file = root.join("src/ns_options.clj");
    client.did_open(&file);
    let text = std::fs::read_to_string(&file).unwrap();

    let (line, ch) = start_of(&text, "(h/greet who)");
    let result = client.goto_definition(&file, line, ch + 4);
    let uri = result["uri"].as_str().expect("expected Location");
    assert!(uri.ends_with("/src/helpers.clj"), "got {}", uri);
}

#[test]
fn test_e2e_prepare_rename_advertises_capability() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    let result = client.initialize(&root);
    assert_eq!(
        result["capabilities"]["renameProvider"]["prepareProvider"],
        json!(true),
        "capabilities: {}",
        result["capabilities"]["renameProvider"]
    );
}

#[test]
fn test_e2e_prepare_rename_returns_token_range() {
    // A local and a project global both report exactly the token the rename
    // would rewrite, so the editor's rename box starts on the right text.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let locals = root.join("src/locals.clj");
    client.did_open(&locals);
    let text = std::fs::read_to_string(&locals).unwrap();
    let (line, ch) = start_of(&text, "base");
    let range = client.prepare_rename(&locals, line, ch + 1);
    assert_eq!(range["start"], json!({ "line": line, "character": ch }));
    assert_eq!(
        range["end"],
        json!({ "line": line, "character": ch + "base".len() as u32 })
    );

    let core = root.join("src/core.clj");
    client.did_open(&core);
    let core_text = std::fs::read_to_string(&core).unwrap();
    let (dline, dch) = start_of(&core_text, "add");
    let range = client.prepare_rename(&core, dline, dch + 1);
    assert_eq!(range["start"], json!({ "line": dline, "character": dch }));
    assert_eq!(
        range["end"],
        json!({ "line": dline, "character": dch + "add".len() as u32 })
    );
}

#[test]
fn test_e2e_prepare_rename_rejects_what_rename_rejects() {
    // Every rejection `rename` makes, `prepareRename` makes with the same
    // message — the editor refuses in place instead of opening a box that
    // fails on submit.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let probe = root.join("src/destructured.clj");
    std::fs::write(
        &probe,
        "(ns simple.destructured)\n(defn f [{:keys [amount]}] amount)\n",
    )
    .unwrap();

    let utils = root.join("src/utils.clj");
    let file = root.join("src/ns_options.clj");
    for path in [&utils, &file, &probe] {
        client.did_open(path);
    }

    let (str_line, str_ch) = position_of(&utils, "(str \"Hello");
    let (kw_line, kw_ch) = start_of(&std::fs::read_to_string(&file).unwrap(), "::cfg/port");
    let (bind_line, bind_ch) = start_of(&std::fs::read_to_string(&probe).unwrap(), "amount]");

    let cases = [
        (&utils, str_line, str_ch + 1, "rename"),
        (&file, kw_line, kw_ch + 3, "keyword"),
        (&probe, bind_line, bind_ch + 2, ":keys"),
    ];
    for (path, line, ch, needle) in cases {
        let prepared = client.prepare_rename_error(path, line, ch);
        assert!(
            prepared.contains(needle),
            "expected {:?} in the rejection at {}:{}, got: {}",
            needle,
            line,
            ch,
            prepared
        );
        let renamed = client.request_expect_error(
            "textDocument/rename",
            json!({
                "textDocument": { "uri": format!("file://{}", path.display()) },
                "position": { "line": line, "character": ch },
                "newName": "renamed"
            }),
        );
        assert_eq!(
            renamed["message"].as_str().unwrap(),
            prepared,
            "prepareRename and rename must reject alike at {}:{}",
            line,
            ch
        );
    }
}

#[test]
fn test_e2e_prepare_rename_on_alias_half_reports_the_name() {
    // A cursor on the `h` of `h/greet` renames `greet`, so prepareRename must
    // report `greet`'s range in *this* file — never the definition's file.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let file = root.join("src/ns_options.clj");
    client.did_open(&file);
    let text = std::fs::read_to_string(&file).unwrap();

    let (line, ch) = start_of(&text, "h/greet");
    let range = client.prepare_rename(&file, line, ch);
    let name_ch = ch + "h/".len() as u32;
    assert_eq!(
        range["start"],
        json!({ "line": line, "character": name_ch }),
        "alias-half prepareRename: {}",
        range
    );
    assert_eq!(
        range["end"],
        json!({ "line": line, "character": name_ch + "greet".len() as u32 })
    );
}

/// A handler that panics must fail that one request, not take the process
/// down: the next request still gets a real answer.
#[test]
fn test_e2e_server_survives_handler_panic() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start_with_str_env(&root, &[("CLJ_PULSE_TEST_PANIC", "1")]);
    client.initialize(&root);

    let error = client.request_expect_error("clojurePulse/__testPanic", json!({}));
    assert!(
        error["code"].as_i64().is_some(),
        "panicking request returned no JSON-RPC error code: {}",
        error
    );

    // The process must still be serving: a normal request answers as usual.
    let utils = root.join("src/utils.clj");
    client.did_open(&utils);
    let (line, ch) = position_of(&utils, "core/add");
    let hover = client.hover(&utils, line, ch);
    assert!(
        !hover.is_null(),
        "hover returned null after a handler panic"
    );
    let value = hover["contents"]["value"].as_str().unwrap();
    assert!(
        value.contains("Adds two numbers."),
        "hover lost its answer after a handler panic: {}",
        value
    );

    // The panic hook records payload and location in server.log, so the same
    // line exists for panics in background tasks, which never reach a handler.
    let log = root.join(".clj-pulse/server.log");
    let deadline = Instant::now() + TIMEOUT;
    let logged = loop {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        if text.contains("panicked at src/server.rs") {
            break text;
        }
        if Instant::now() >= deadline {
            break text;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        logged.contains("panicked at src/server.rs")
            && logged.contains("deliberate panic from clojurePulse/__testPanic"),
        "server.log has no panic line with a location: {}",
        logged
    );
}

// ---------------------------------------------------------------------------
// Malformed input: every handler returns, none takes the server down.
// ---------------------------------------------------------------------------

/// Builds a bare temp project with the given `deps.edn` contents and one
/// source file, for the manifest tests that need a broken manifest on disk.
fn malformed_deps_project(deps_edn: &str) -> tempfile::TempDir {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("deps.edn"), deps_edn).unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src/ok.clj"),
        "(ns broken.ok)\n\n(defn only-fn\n  \"The one thing this project defines.\"\n  [x]\n  (inc x))\n",
    )
    .unwrap();
    tmp
}

/// A buffer the user is mid-way through typing does not parse. Every
/// position-based handler must still answer rather than error or hang.
#[test]
fn test_e2e_malformed_unbalanced_buffer_still_answers() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let half_typed = root.join("src/half_typed.clj");
    std::fs::write(
        &half_typed,
        "(ns simple.half-typed\n  (:require [simple.core :as core]))\n\n(defn f [x] (let [y (core/add\n",
    )
    .unwrap();
    client.did_open(&half_typed);

    let (line, ch) = position_of(&half_typed, "core/add");
    // None of these may error; a null hover or an empty list is a fine answer.
    let _ = client.hover(&half_typed, line, ch);
    let _ = client.completion_items(&half_typed, line, ch);
    let _ = client.goto_definition(&half_typed, line, ch);

    // Positions inside the unterminated form answer too.
    let last_line = 3;
    let _ = client.hover(&half_typed, last_line, 20);
    let _ = client.document_symbols(&half_typed);

    // And a healthy file in the same session is unaffected.
    let utils = root.join("src/utils.clj");
    client.did_open(&utils);
    let (line, ch) = position_of(&utils, "core/add");
    let hover = client.hover(&utils, line, ch);
    assert!(
        hover["contents"]["value"]
            .as_str()
            .unwrap_or_default()
            .contains("Adds two numbers."),
        "an unbalanced buffer broke a healthy one: {}",
        hover
    );
}

/// A generated or minified source file can be one enormous line. It must not
/// blow up the position math or the parser.
#[test]
fn test_e2e_malformed_huge_single_line_answers() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let huge = root.join("src/huge.clj");
    let mut text = String::from("(ns simple.huge) (def big \"");
    text.push_str(&"abcdefgh".repeat(4 * 1024 * 1024 / 8));
    text.push_str("\")\n");
    assert!(text.len() > 4 * 1024 * 1024, "fixture is not 4 MB");
    std::fs::write(&huge, &text).unwrap();
    client.did_open(&huge);

    let symbols = client.document_symbols(&huge);
    assert!(
        symbols.is_array(),
        "documentSymbol on a 4 MB line did not answer with a list: {}",
        symbols
    );
    // A position far along that single line answers too.
    let _ = client.hover(&huge, 0, 2_000_000);
}

/// A source file that is not valid UTF-8 is skipped with a log line; the rest
/// of the project still indexes and answers.
#[test]
fn test_e2e_malformed_non_utf8_file_is_skipped() {
    let project = setup_named("malformed_project");
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let ok = root.join("src/ok.clj");
    client.did_open(&ok);
    let (line, ch) = position_of(&ok, "well-formed");
    let hover = client.hover(&ok, line, ch);
    assert!(
        hover["contents"]["value"]
            .as_str()
            .unwrap_or_default()
            .contains("perfectly ordinary function"),
        "the readable file did not index alongside the unreadable one: {}",
        hover
    );

    let log = std::fs::read_to_string(root.join(".clj-pulse/server.log")).unwrap_or_default();
    assert!(
        log.contains("failed to read") && log.contains("bad_bytes.clj"),
        "the unreadable file was not logged as skipped: {}",
        log
    );
}

/// An empty `deps.edn` has no `:paths`; the server falls back to `src`/`test`
/// and indexes the project anyway.
#[test]
fn test_e2e_malformed_empty_deps_edn_still_indexes() {
    let project = malformed_deps_project("{}\n");
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let ok = root.join("src/ok.clj");
    client.did_open(&ok);
    let (line, ch) = position_of(&ok, "only-fn");
    let hover = client.hover(&ok, line, ch);
    assert!(
        hover["contents"]["value"]
            .as_str()
            .unwrap_or_default()
            .contains("The one thing this project defines."),
        "an empty deps.edn stopped src/ from indexing: {}",
        hover
    );
}

/// A `deps.edn` that does not parse must not stop the server: it initializes,
/// falls back to the default source paths, and answers.
#[test]
fn test_e2e_malformed_invalid_deps_edn_still_answers() {
    let project = malformed_deps_project("{:paths [\n");
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let ok = root.join("src/ok.clj");
    client.did_open(&ok);
    let (line, ch) = position_of(&ok, "only-fn");
    // The truncated `:paths` may or may not yield `src`, so the contract is
    // only that the request returns instead of erroring or hanging.
    let _ = client.hover(&ok, line, ch);
    let symbols = client.document_symbols(&ok);
    assert!(
        symbols.is_array(),
        "a broken deps.edn broke documentSymbol: {}",
        symbols
    );
}

/// A `didChange` whose range is past the end of the document is dropped, and
/// the next request still answers off the last good text.
#[test]
fn test_e2e_malformed_did_change_past_end_of_document() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);

    let utils = root.join("src/utils.clj");
    client.did_open(&utils);

    client.did_change_range(&utils, 2, (9_999, 0), (9_999, 5), "nonsense");

    let (line, ch) = position_of(&utils, "core/add");
    let hover = client.hover(&utils, line, ch);
    assert!(
        hover["contents"]["value"]
            .as_str()
            .unwrap_or_default()
            .contains("Adds two numbers."),
        "an out-of-range didChange cost the buffer its answers: {}",
        hover
    );

    // A well-formed edit after the bad one still applies. utils.clj ends with
    // a newline, so line 10 is the empty line past the last form.
    client.did_change_insert(&utils, 10, 0, "(defn later [] 1)\n");
    let symbols = client.document_symbols(&utils);
    let names: Vec<&str> = symbols
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        names.contains(&"later"),
        "edits stopped applying after an out-of-range one: {:?}",
        names
    );
}

/// A panicking handler leaves its id behind in tower-lsp's pending-request map,
/// because the map is only cleared when the handler future returns. The guard
/// clears it, so a client that reuses request ids keeps working.
#[test]
fn test_e2e_panicked_request_id_can_be_reused() {
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start_with_str_env(&root, &[("CLJ_PULSE_TEST_PANIC", "1")]);
    client.initialize(&root);

    let utils = root.join("src/utils.clj");
    client.did_open(&utils);
    let (line, ch) = position_of(&utils, "core/add");

    // Ids well past the harness counter, so nothing else claims them.
    let panicked = client.request_with_id(9001, "clojurePulse/__testPanic", json!({}));
    assert!(
        panicked.get("error").is_some(),
        "expected an error: {}",
        panicked
    );

    // No request in between: the guard must clear the id before it dispatches
    // the reusing request, not merely before some later one.
    let reused = client.request_with_id(
        9001,
        "textDocument/hover",
        json!({
            "textDocument": { "uri": format!("file://{}", utils.display()) },
            "position": { "line": line, "character": ch }
        }),
    );
    assert!(
        reused.get("error").is_none(),
        "reusing a panicked request id was rejected: {}",
        reused
    );
    assert!(
        reused["result"]["contents"]["value"]
            .as_str()
            .unwrap_or_default()
            .contains("Adds two numbers."),
        "reused id returned no hover: {}",
        reused
    );
}

#[test]
fn test_e2e_completion_keywords_auto_resolved() {
    // `::` in a file that uses `::local` offers it back, as `::local` — the
    // notation being typed — with an edit that replaces the marker too.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("Indexed");

    let keywords = root.join("src/keywords.clj");
    client.did_open(&keywords);

    let last_line = std::fs::read_to_string(&keywords).unwrap().lines().count() as u32;
    client.did_change_insert(&keywords, last_line, 0, "(def z ::lo)");
    let items = client.completion_items(&keywords, last_line, 11);

    let item = items
        .as_array()
        .expect("completion items")
        .iter()
        .find(|i| i["label"] == "::local")
        .unwrap_or_else(|| panic!("`::local` not offered: {}", items));
    assert_eq!(item["kind"], 14, "keyword kind: {}", item);
    let range = &item["textEdit"]["range"];
    assert_eq!(
        range["start"]["character"], 7,
        "edit starts at `::`: {}",
        item
    );
    assert_eq!(
        range["end"]["character"], 11,
        "edit spans the token: {}",
        item
    );
    assert_eq!(item["textEdit"]["newText"], "::local");
}

#[test]
fn test_e2e_completion_keywords_by_frequency() {
    // The `:` trigger with nothing typed: every keyword the project uses,
    // most-used first (`:id` three times, `:name` once).
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("Indexed");

    let utils = root.join("src/utils.clj");
    client.did_open(&utils);

    let last_line = std::fs::read_to_string(&utils).unwrap().lines().count() as u32;
    client.did_change_insert(&utils, last_line, 0, "(def z :)");
    let items = client.completion_items(&utils, last_line, 8);

    let labels: Vec<&str> = items
        .as_array()
        .expect("completion items")
        .iter()
        .filter_map(|i| i["label"].as_str())
        .collect();
    let id = labels.iter().position(|l| *l == ":id");
    let name = labels.iter().position(|l| *l == ":name");
    assert!(
        id.is_some() && name.is_some() && id < name,
        "expected `:id` before `:name`: {:?}",
        labels
    );
    // Keyword completion answers with keywords only — no vars.
    assert!(
        labels.iter().all(|l| l.starts_with(':')),
        "non-keyword offered after `:`: {:?}",
        labels
    );
}

#[test]
fn test_e2e_completion_keyword_mid_token_replaces_whole_token() {
    // Cursor mid-token (`:na|me`): applying the edit must yield `:name`, not
    // `:nameme`.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("Indexed");

    let utils = root.join("src/utils.clj");
    client.did_open(&utils);

    let line_text = "(def z :name)";
    let last_line = std::fs::read_to_string(&utils).unwrap().lines().count() as u32;
    client.did_change_insert(&utils, last_line, 0, line_text);
    // Right after `:na`.
    let items = client.completion_items(&utils, last_line, 10);

    let item = items
        .as_array()
        .expect("completion items")
        .iter()
        .find(|i| i["label"] == ":name")
        .unwrap_or_else(|| panic!("`:name` not offered: {}", items));
    let range = &item["textEdit"]["range"];
    let start = range["start"]["character"].as_u64().unwrap() as usize;
    let end = range["end"]["character"].as_u64().unwrap() as usize;
    let new_text = item["textEdit"]["newText"].as_str().unwrap();
    let applied = format!("{}{}{}", &line_text[..start], new_text, &line_text[end..]);
    assert_eq!(applied, "(def z :name)", "applying the edit: {}", item);
}

/// Puts a minimal `clojure.string` on the project's cached classpath. The
/// committed fixture `.cpcache` names jars from the machine that generated it,
/// so a test needing a library on the classpath supplies its own — otherwise
/// stage 2 finds nothing and never reports `library indexing complete`.
fn write_clojure_string_jar(root: &std::path::Path) {
    let jar_path = root.join("clojure-string.jar");
    let jar_file = std::fs::File::create(&jar_path).unwrap();
    let mut zip = zip::ZipWriter::new(jar_file);
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("clojure/string.clj", opts).unwrap();
    zip.write_all(
        b"(ns clojure.string)\n\n(defn join\n  \"Joins a collection.\"\n  [sep coll]\n  sep)\n",
    )
    .unwrap();
    zip.finish().unwrap();

    let cpcache = root.join(".cpcache");
    std::fs::create_dir_all(&cpcache).unwrap();
    std::fs::write(
        cpcache.join("clojure-string.cp"),
        jar_path.display().to_string(),
    )
    .unwrap();
}

#[test]
fn test_e2e_completion_auto_require_inserts_require() {
    // `str/jo` in a file that never required clojure.string: the item comes
    // with the edit that inserts the require into the ns form.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();
    write_clojure_string_jar(&root);

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("Indexed");
    client.wait_for_log("library indexing complete");

    let utils = root.join("src/utils.clj");
    client.did_open(&utils);

    let last_line = std::fs::read_to_string(&utils).unwrap().lines().count() as u32;
    client.did_change_insert(&utils, last_line, 0, "(str/jo)");
    let items = client.completion_items(&utils, last_line, 7);

    let item = items
        .as_array()
        .expect("completion items")
        .iter()
        .find(|i| i["label"] == "str/join")
        .unwrap_or_else(|| panic!("`str/join` not offered: {}", items));
    let edits = item["additionalTextEdits"]
        .as_array()
        .unwrap_or_else(|| panic!("no additionalTextEdits: {}", item));
    assert_eq!(edits.len(), 1, "one require edit: {}", item);
    assert!(
        edits[0]["newText"]
            .as_str()
            .unwrap()
            .contains("[clojure.string :as str]"),
        "edit text: {}",
        edits[0]
    );
    // The ns form is the first two lines of utils.clj; the edit appends to its
    // `(:require …)` clause rather than landing in the body.
    assert_eq!(edits[0]["range"]["start"]["line"], 1, "edit: {}", edits[0]);
}

#[test]
fn test_e2e_completion_no_auto_require_when_already_required() {
    // The same completion in a file that already requires clojure.string comes
    // through the ordinary alias path, with no edit attached.
    let project = setup_project();
    let root = project.path().canonicalize().unwrap();

    write_clojure_string_jar(&root);

    let f = root.join("src/has_str.clj");
    std::fs::write(
        &f,
        "(ns simple.has-str\n  (:require [clojure.string :as str]))\n\n\
         (defn shout [xs] (str/join xs))\n",
    )
    .unwrap();

    let mut client = LspClient::start(&root);
    client.initialize(&root);
    client.wait_for_log("Indexed");
    client.wait_for_log("library indexing complete");
    client.did_open(&f);

    let last_line = std::fs::read_to_string(&f).unwrap().lines().count() as u32;
    client.did_change_insert(&f, last_line, 0, "(str/jo)");
    let items = client.completion_items(&f, last_line, 7);

    let item = items
        .as_array()
        .expect("completion items")
        .iter()
        .find(|i| i["label"] == "str/join")
        .unwrap_or_else(|| panic!("`str/join` not offered: {}", items));
    assert!(
        item["additionalTextEdits"].is_null(),
        "an already-required namespace must carry no require edit: {}",
        item
    );
}
