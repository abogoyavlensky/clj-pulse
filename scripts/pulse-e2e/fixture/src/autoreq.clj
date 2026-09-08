(ns autoreq
  (:require [other :as o]))

(defn run [s]
  (o/helper 1)
  (slugi s))

(def config {:id 1})
