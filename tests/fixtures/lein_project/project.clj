(defproject lein-app "0.1.0-SNAPSHOT"
  :description "e2e fixture: a Leiningen project resolved without java"
  ;; :local-repo keeps the test hermetic — the Maven repo lives inside the
  ;; copied temp project at <root>/m2.
  :local-repo "m2"
  :dependencies [[mylib "1.0.0"]]
  :source-paths ["src"]
  ;; Metadata and a regex literal that plain EDN parsing would choke on; the
  ;; masked per-vector parser must still resolve :dependencies above.
  :clean-targets ^{:protect false} [:target-path]
  :profiles {:coverage {:cloverage {:ns-exclude-regex [#"user"]}}})
