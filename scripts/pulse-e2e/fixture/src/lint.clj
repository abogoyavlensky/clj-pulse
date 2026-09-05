(ns lint
  (:require [clojure.set :as set]))

(defn describe
  "Never touches the required namespace, so `clojure.set` is unused."
  [x]
  (str "value: " x))
