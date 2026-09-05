(ns simple.config
  (:require [integrant.core :as ig]))

(defmethod ig/init-key ::port
  [_ opts]
  (or opts 8080))
