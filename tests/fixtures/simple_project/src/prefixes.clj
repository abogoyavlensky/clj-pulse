(ns simple.prefixes
  (:require [simple.prefix-target :as c]))

(def pfx-state (atom 0))
(defn read-it [] @pfx-state)
(defn var-of [] #'pfx-state)
(defmacro pfx-m [] `(deref ~pfx-state))
(defmacro pfx-n [] `(do `pfx-state ~@pfx-state))
(defn local-deref [] (let [pfx-a (atom 1)] @pfx-a))
(defn var-via-alias [] #'c/pfx-target)
(defn pfx-shadow [] (let [pfx-state 2] [#'pfx-state pfx-state]))
(defn quoted [] 'simple.prefixes/pfx-state)
