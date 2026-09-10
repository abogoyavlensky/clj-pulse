(ns my.app
  (:require [malli.util :as mu]
            [schema.core :as s]
            [clojure.spec.alpha :as spec]))

(mu/defn f :- :int
  [x :- :int]
  (str x))

(s/defn g [y] y)

(mu/defn- h [] 1)

(defmulti m :kind)

(mu/defmethod m :k [_] 1)

(spec/def ::user string?)
