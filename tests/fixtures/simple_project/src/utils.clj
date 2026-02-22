(ns simple.utils
  (:require [simple.core :as core]))

(defn add-and-double
  "Adds two numbers then doubles the result."
  [x y]
  (* 2 (core/add x y)))

(defn greet [name]
  (str "Hello, " name))
