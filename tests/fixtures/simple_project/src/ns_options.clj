(ns simple.ns-options
  "Exercises the ns-form options: :as-alias, :refer-clojure, prefix lists."
  (:refer-clojure :exclude [update] :rename {map cmap})
  (:require [simple.config :as-alias cfg]
            (simple [helpers :as h])))

(declare only-declared)

(declare defined-later)

(defn defined-later [x]
  (only-declared x))

(defn port [system]
  (get system ::cfg/port))

(defn shout [who]
  (h/greet who))

(defn update [m]
  (cmap inc (defined-later m)))
