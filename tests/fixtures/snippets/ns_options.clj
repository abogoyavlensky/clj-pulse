(ns my.app.handlers
  (:require [my.app.config :as-alias cfg]))

(defn port [system]
  (get system ::cfg/port))
