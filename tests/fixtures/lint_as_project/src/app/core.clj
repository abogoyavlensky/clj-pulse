(ns app.core
  (:require [app.macros :refer [defthing]]))

(defn use-it [] widget)

(defthing widget 1)
