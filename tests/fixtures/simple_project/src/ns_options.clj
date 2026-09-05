(ns simple.ns-options
  "Exercises the ns-form options: :as-alias, :refer-clojure, prefix lists."
  (:refer-clojure :exclude [update] :rename {map cmap})
  (:require [simple.config :as-alias cfg]
            (clojure [string :as s])))

(declare only-declared)

(declare defined-later)

(defn defined-later [x]
  (only-declared x))

(defn port [system]
  (get system ::cfg/port))

(defn shout [xs]
  (s/upper-case (first (cmap str xs))))

(defn update [m]
  (defined-later m))
