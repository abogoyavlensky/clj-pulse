(ns simple.records-consumer
  (:require [simple.records :as rec :refer [->Point]]))

(defn build []
  [(rec/map->Point {})
   (->Point 0 0)
   (rec/->Cell 1)])
