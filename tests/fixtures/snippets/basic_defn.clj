(ns my.core)

(def PI 3.14159)

(defn hello
  "Says hello to someone."
  [name]
  (str "Hello, " name))

(defn- private-thing [x] x)

(defmacro when-pos [n & body]
  `(when (pos? ~n) ~@body))
