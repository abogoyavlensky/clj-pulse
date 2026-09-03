(ns ^{:doc "Tests in every require style."} my.core-test
  (:require [clojure.test :refer :all]
            [clojure.string :as str]
            #?(:clj [clojure.test :as t] :cljs [cljs.test :as t :include-macros true]))
  (:use [clojure.set]))

(deftest refer-all-style (is (= 1 1)))

(t/deftest alias-style (is (str/blank? "")))

(clojure.test/deftest qualified-style (is true))

(deftest- private-style (is true))

(deftest with-body
  (testing "nested"
    (is (= 2 (+ 1 1)))))
