(ns my.arity)

(defn greet
  "Greets with optional title."
  ([name] (greet nil name))
  ([title name] (str title " " name)))
