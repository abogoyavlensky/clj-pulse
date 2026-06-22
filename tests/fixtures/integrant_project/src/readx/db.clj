(ns readx.db
  (:require [integrant.core :as ig]))

(defmethod ig/assert-key ::db
  [_ params]
  params)

(defmethod ig/init-key ::db
  [_ opts]
  opts)

(defmethod ig/halt-key! ::db
  [_ datasource]
  (when datasource :closed))
