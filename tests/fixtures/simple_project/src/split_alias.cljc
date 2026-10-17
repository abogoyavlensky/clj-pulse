(ns simple.split-alias
  (:require #?(:clj [simple.split-jvm :as x] :cljs [goog.object :as x])))

(defn call-split [] (x/split-f))
