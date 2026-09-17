(ns simple.locals (:require [clojure.test :refer [deftest are]]))

(defn compute [n]
  (let [base   (inc n)
        scaled (* base 2)]
    (+ base scaled)))

(deftest compute-table
  (are [input expected] (= expected (compute input))
    1 4
    2 6))
