(ns app.core
  (:require [app.macros :refer [defthing programs]]))

(defn use-it [] widget)

(defthing widget 1)

(programs rm mv)

(defn clean [] (rm "-rf"))
