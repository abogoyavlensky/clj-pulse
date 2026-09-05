(ns my.app.handlers
  (:refer-clojure :exclude [update] :rename {map cmap})
  (:require [my.app.config :as-alias cfg]
            [clojure.string :refer [join] :rename {join str-join}]))

(defn port [system]
  (get system ::cfg/port))

(defn update [m]
  (cmap inc m))

(defn render [xs]
  (str-join "," xs))

(defn go [m]
  (update m))
