#!/usr/bin/env bb
;; Run: bb scripts/gen_core.clj > src/index/core.rs

(println "use super::CoreSymbol;")
(println)
(println "pub fn core_symbols() -> Vec<CoreSymbol> {")
(println "    vec![")

(doseq [[sym-name var] (sort-by key (ns-publics 'clojure.core))
        :let [m (meta var)
              params (str (:arglists m))
              doc (or (:doc m) "")
              doc-escaped (-> doc
                             (clojure.string/replace "\\" "\\\\")
                             (clojure.string/replace "\"" "\\\"")
                             (clojure.string/replace "\n" "\\n"))
              params-escaped (-> params
                                (clojure.string/replace "\"" "\\\""))]]
  (println (str "        CoreSymbol { name: \"" sym-name
                "\".to_string(), params: \"" params-escaped
                "\".to_string(), doc: \"" doc-escaped
                "\".to_string() },")))

(println "    ]")
(println "}")
