(ns simple.keywords
  (:require [simple.core :as c]))

(def defaults
  {:id 0
   :name "anon"
   ::local true})

(defn lookup [m]
  (or (:id m) (::local m) (get m :simple.core/x)))

(defn tagged [m]
  (assoc m :id (::c/thing m)))
