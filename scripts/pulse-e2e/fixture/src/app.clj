(ns app
  (:require [clojure.data.json :as json]
            [other :as o]))

(defn run []
  (o/helper 1)
  (json/write-str {:a 1}))
