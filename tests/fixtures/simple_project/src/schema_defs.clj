(ns simple.schema-defs
  "A qualified def-family head: `mu/defn` defines a function the way
  `clojure.core/defn` does, whatever namespace the macro comes from."
  (:require [malli.util :as mu]))

(def factor 10)

(mu/defn scale [factor x]
  (* factor x))

(defn scale-all [xs]
  (map #(scale factor %) xs))
