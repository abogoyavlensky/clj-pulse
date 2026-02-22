(ns my.service
  (:require [clojure.string :as str]
            [my.core :as core]
            [my.utils :refer [format-date parse-id]]))

(defn process [input]
  (str/upper-case input))
