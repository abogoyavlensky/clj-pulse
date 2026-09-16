(ns simple.alias-sites
  (:require [simple.core :as c]))

(c/blend 1 2)
(resolve 'c/blend)
(def k ::c/site)
(defn f [{::c/keys [blend]}] blend)
(def m #::c{:site 1})
(def data {:keys [c/blend]})
(defn g [{:keys [c/x]}] x)
(def lit :c/site)
(let [c 1] c)
