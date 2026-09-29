(ns simple.multi)

(defmulti area :shape)

(defmethod area :circle [{:keys [r]}]
  (* 3 r r))

(defmethod area :square [{:keys [side]}]
  (* side side))

(defn circle-area []
  (area {:shape :circle :r 2}))
