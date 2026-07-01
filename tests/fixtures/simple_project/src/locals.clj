(ns simple.locals)

(defn compute [n]
  (let [base   (inc n)
        scaled (* base 2)]
    (+ base scaled)))
