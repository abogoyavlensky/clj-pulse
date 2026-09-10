(ns my.app
  (:require [malli.util :as mu]
            [schema.core :as s]
            [clojure.spec.alpha :as spec]))

(mu/defn f :- :int
  [x :- :int]
  (str x))

(s/defn g [y] y)

(s/defn annotated :- s/Str
  [y :- s/Int]
  y)

(mu/defn vector-schema :- [:vector :int]
  [xs]
  xs)

(s/defn seq-schema :- [s/Int]
  "Doc after the schema."
  [zs]
  zs)

(s/defn doc-first
  "Doc before the schema."
  :- [s/Int]
  [ws]
  ws)

(mu/defn- h [] 1)

(defmulti m :kind)

(mu/defmethod m :k [_] 1)

(spec/def ::user string?)
