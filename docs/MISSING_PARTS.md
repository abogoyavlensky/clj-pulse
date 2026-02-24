From step 2
⏺ For V1 scope (definition, completion, hover on project source), it covers the most common patterns. But it's not exhaustive. Some things it doesn't handle:                                                                                   
                                                                                                                                                                                                                                                
  Minor gaps (likely worth adding):                                                                                                                                                                                                             
  - :require with :as-alias (Clojure 1.11+)                                                                                                                                                                                                     
                                                                                                                                                                                                                                                
  Edge cases (probably fine to skip for V1):
  - ns with :use, :import, :refer-clojure
  - Metadata-based privacy like (def ^:private foo ...) — only defn- is detected
  - declare forward declarations
  - Defs inside (comment ...) blocks (shouldn't be indexed anyway)
  - Nested defs (bad practice but exists in the wild)

  Not needed for V1 per plan:
  - Dependency JAR parsing
  - Macro-expanded forms
  - defmethod dispatch values


  From step 5:
  - strange annotation of autocompleted functions - double rounded brackets (([...]))


From step 7:
- debounced didChange re-indexing (200-300ms) for better UX