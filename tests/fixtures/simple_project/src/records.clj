(ns simple.records)

(defrecord Point [x y])

(deftype Cell [v])

(defn points []
  [(Point. 1 2)
   (->Point 3 4)
   (map->Point {:x 5})
   (Cell. 1)
   (->Cell 2)])
