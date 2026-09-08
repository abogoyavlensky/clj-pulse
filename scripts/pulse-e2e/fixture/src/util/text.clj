(ns util.text)

(defn slugify
  "Lower-cases and dashes a string."
  [s]
  (clojure.string/replace (clojure.string/lower-case s) " " "-"))
