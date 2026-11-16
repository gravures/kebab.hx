;; split-overlay.scm — split-anchored overlay components for Steel Helix
;;
;; A pushed component receives the full terminal rect and renders on top
;; of all other layers — anything you don't explicitly paint shows through
;; from underneath.
;;
;; The component's name is how every close/remove path finds it.
;;
;; `editor-focused-buffer-area` is the focused split's rect minus the
;; bufferline and command line, already clipped (e.g. forest's sidebar).
;; It's focus-bound, so an overlay that must stay on its split caches this
;; rect per overlay (It's re-read every frame while focused, reuse while blurred,
;; clamped to the terminal, and self-healed on refocus).
;;
;; Usage:
;;   (require "split-overlay.scm")
;;   (define (my-overlay-open)
;;     (unless (split-overlay-open? "my-overlay")
;;       (split-overlay-open! "my-overlay" state render-fn event-fn)))

(require "helix/editor.scm")
(require "helix/components.scm")
(require "helix/misc.scm") ; push-component!, pop-last-component-by-name!

(provide split-overlay-open? split-overlay-open! split-overlay-close!
  split-overlay-toggle!
  split-overlay-region
  split-overlay-render-native!
  split-overlay-bg-style
  split-overlay-anchor-view)

;; ------------------------------------------------------------------ types

;;@doc
;; An open overlay record.
;;
;; ```scheme
;; (struct Overlay (anchor-doc anchor-view open on-close cursor-fn area)) -> Overlay?
;; ```
;; #### Arguments
;; - `anchor-doc` (`DocumentId?`) - document ID of the anchor view at open time.
;; - `anchor-view` (`ViewId`) - view ID of the anchor view at open time
;;    (focused view, not doc-in-view?).
;; - `open` (`box?`) - mutable flag, `#t` while overlay is on the component stack.
;; - `on-close` (`(or/c (-> void?) #f)`) - optional callback run exactly once on close.
;; - `cursor-fn` (`(or/c (-> any/c Rect?) Position?) #f)`) - optional cursor handler for the component.
;; - `area` (`box?`) - cached `Rect?` from `editor-focused-buffer-area`,
;;    updated when anchor is focused.
(struct Overlay (anchor-doc anchor-view open on-close cursor-fn area))
;; NOTE: The anchor view is captured at open via `editor-focus`. With the same document
;; in two splits, `doc-in-view?` returns the first traversal hit,
;; which may not be the pane the user opened the overlay from.
(define *overlays* '())

(define (%lookup name lst)
  (cond
    [(null? lst) #f]
    [(equal? name (car (car lst))) (cdr (car lst))]
    [else (%lookup name (cdr lst))]))

;; ------------------------------------------------------------------ lifecycle

;;@doc
;; Check if an overlay of this name is currently open.
;;
;; ```scheme
;; (split-overlay-open? name) -> boolean?
;; ```
;;
;; #### Arguments
;; - `name` (`string?`) - overlay name.
;;
;; #### Returns
;; `boolean?`: `#t` if the overlay is on the component stack, `#f` otherwise.
;;
;; #### Examples
;; ```scheme
;; (split-overlay-open? "my-overlay") ;; => #f
;; (split-overlay-open! "my-overlay" '() (lambda _ #f) (lambda _ (event-result/ignore)))
;; (split-overlay-open? "my-overlay") ;; => #t
;; ```
(define (split-overlay-open? name)
  (let ([e (%lookup name *overlays*)])
    (and e (unbox (Overlay-open e)))))

;;@doc
;; Push a named overlay component anchored to the currently focused view.
;;
;; ```scheme
;; (split-overlay-open! name state render-fn event-fn [cursor-fn #f] [on-close #f]) -> void?
;; ```
;;
;; #### Arguments
;; - `name` (`string?`) - unique overlay name; must match the component name (never wrap in `overlaid`).
;; - `state` (`any/c`) - arbitrary state passed to `render-fn` and `event-fn`.
;; - `render-fn` (`(-> any/c Rect? Buffer?)`) - render callback, called each frame with state, render rect, and buffer.
;; - `event-fn` (`(-> any/c Event?) SteelEventResult?`) - event handler callback for the overlay.
;; - `cursor-fn` (`(or/c (-> any/c Rect?) Position?) #f)`) - optional cursor position callback.
;; - `on-close` (`(or/c (-> void?) #f)`) - optional callback run exactly once when the overlay closes.
;;
;; #### Returns
;; `void?`
;;
;; #### Notes
;; The anchor view is captured at open via `editor-focus`. The overlay is closes when
;; its anchor document leaves the screen entirely (closed or swapped out of every view).
;; The name must stay attached to the component itself.
;;
;; #### Examples
;; ```scheme
;; (define (open-find-overlay)
;;   (unless (split-overlay-open? "find")
;;     (split-overlay-open! "find"
;;       (hash 'query "")
;;       (lambda (st rect buf) ...)
;;       (lambda (st ev) ...))))
;; ```
(define (split-overlay-open! name state render-fn event-fn [cursor-fn #f] [on-close #f])
  (let* ([focus (editor-focus)]
         [doc (editor->doc-id focus)]
         [entry (Overlay doc focus (box #t) on-close cursor-fn
                 (box (editor-focused-buffer-area)))]
         [fns (if cursor-fn
               (hash "handle_event" event-fn "cursor" cursor-fn)
               (hash "handle_event" event-fn))])
    (push-component! (new-component! name state render-fn fns))
    (set! *overlays* (cons (cons name entry) *overlays*))))

;;@doc
;; Close an overlay by name, popping its component and running its `on-close`
;; function.
;;
;; ```scheme
;; (split-overlay-close! name) -> void?
;; ```
;;
;; #### Arguments
;; - `name` (`string?`) - overlay name.
;;
;; #### Returns
;; `void?`
;;
;; #### Notes
;; Safe to call when the overlay is not open. The `on-close` function runs exactly once, whichever path closes the overlay (explicit
;; close, document removed, or auto-close hook).
;;
;; #### Examples
;; ```scheme
;; (split-overlay-close! "find")
;; ```
(define (split-overlay-close! name)
  (let ([e (%lookup name *overlays*)])
    (when (and e (unbox (Overlay-open e)))
      (set-box! (Overlay-open e) #f)
      (pop-last-component-by-name! name)
      (let ([f (Overlay-on-close e)])
        (when f (f))))))

;;@doc
;; Get the anchor view for an overlay, if open.
;;
;; ```scheme
;; (split-overlay-anchor-view name) -> (or/c ViewId #f)
;; ```
(define (split-overlay-anchor-view name)
  (let ([e (%lookup name *overlays*)])
    (and e (Overlay-anchor-view e))))

;;@doc
;; Toggle an overlay by running the appropriate open or close function.
;;
;; ```scheme
;; (split-overlay-toggle! name open! close!) -> boolean?
;; ```
;;
;; #### Arguments
;; - `name` (`string?`) - overlay name.
;; - `open!` (`(-> void?)`) - callback to open the overlay.
;; - `close!` (`(-> void?)`) - callback to close the overlay.
;;
;; #### Returns
;; `boolean?`: `#t` if the overlay was opened, `#f` if it was closed.
;;
;; #### Examples
;; ```scheme
;; (split-overlay-toggle! "find"
;;   (lambda () (split-overlay-open! "find" ...))
;;   (lambda () (split-overlay-close! "find")))
;; ```
(define (split-overlay-toggle! name open! close!)
  (if (split-overlay-open? name) (begin (close!) #f) (begin (open!) #t)))

;; Hook to serve all overlays. It runs on `post-command`, not on focus
;; changes — moving splits must not close overlays. Only the document
;; leaving the screen entirely closes an overlay (closed or swapped
;; out of every view).
;; The hook runs right after open-tool, so a check that's false
;; at open would close the overlay immediately.
(register-hook 'post-command
  (lambda (_command)
    (define (check lst)
      (unless (null? lst)
        (let ([name (car (car lst))]
              [e (cdr (car lst))])
          (when (and (unbox (Overlay-open e))
                 (not (editor-doc-in-view? (Overlay-anchor-doc e))))
            (split-overlay-close! name))
          (check (cdr lst)))))
    (check *overlays*)))

;; ------------------------------------------------------------------ placement

;;@doc
;; Background style for overlays, using the editor's `ui.background` theme scope.
;;
;; ```scheme
;; (split-overlay-bg-style) -> Style?
;; ```
;;
;; #### Returns
;; `Style?`: a style with background set to the theme's `ui.background` color.
;;
;; #### Notes
;; Constructed once at module load. If the theme changes at runtime, this style
;; does not update — consumers needing live theme response should build their own.
(define split-overlay-bg-style
  (let ([style (style)])
    (set-style-bg! style (style->bg (theme-scope "ui.background")))
    style))

;; The view's rect minus that view's statusline row, then clamped to the
;; terminal rect we are rendered on, so a rect cached before a resize or a
;; layout change can never paint outside the screen.
(define (%region-of view-rect rect)
  (if (not view-rect)
    (%fallback-region rect)
    (let* ([h (max 1 (- (area-height view-rect) 1))]
           [x (max (area-x view-rect) (area-x rect))]
           [y (max (area-y view-rect) (area-y rect))]
           [r (min (+ (area-x view-rect) (area-width view-rect))
               (+ (area-x rect) (area-width rect)))]
           [b (min (+ (area-y view-rect) h)
               (+ (area-y rect) (area-height rect)))])
      (area x y (max 1 (- r x)) (max 1 (- b y))))))

(define (%fallback-region rect)
  (area (area-x rect) (area-y rect)
    (area-width rect)
    (max 1 (- (area-height rect) 2))))

;;@doc
;; Get the paintable region for an overlay.
;;
;; Its anchor view's rect minus the statusline row.
;;
;; ```scheme
;; (split-overlay-region name rect) -> Rect?
;; ```
;;
;; #### Arguments
;; - `name` (`string?`) - overlay name.
;; - `rect` (`Rect?`) - the terminal rect the component is rendered on (full terminal).
;;
;; #### Returns
;; `Rect?`: the clamped region the overlay may cover. Never covers the bufferline row,
;; the command-line row, either split's statusline, the other split, or anything an
;; editor clip reserves. Stays on the anchor view when focus moves elsewhere.
;;
;; #### Notes
;; When the anchor view is focused, the region is re-read every frame (picking up
;; resizes, splits, and clip changes). When blurred, the last cached rect is reused
;; (stale only after a layout change made while blurred; self-heals on refocus).
;;
;; #### Examples
;; ```scheme
;; (define (render-fn state rect buf)
;;   (let ([region (split-overlay-region "my-overlay" rect)])
;;     ...))
;; ```
(define (split-overlay-region name rect)
  (let ([e (%lookup name *overlays*)])
    (cond
      ;; anchor focused: re-read every frame, so resizes, splits and clip
      ;; changes are picked up while the view can actually be read
      [(and e (equal? (editor-focus) (Overlay-anchor-view e)))
        (let ([a (editor-focused-buffer-area)])
          (when a (set-box! (Overlay-area e) a))
          (%region-of (or a (unbox (Overlay-area e))) rect))]
      ;; anchor blurred: reuse the last rect read (stale only after a layout
      ;; change made while blurred; self-heals on refocus)
      [e (%region-of (or (unbox (Overlay-area e))
                      (editor-focused-buffer-area))
          rect)]
      [else (%fallback-region rect)])))

;; Natural-size memo
;;
;; Keyed on (content cache-key, available width): a native
;; component's required_size re-parses its whole content, and is asked once
;; per resize.
;;
;; NOTE: the memo key is the consumer's cache-key alone (the overlay name
;; used to namespace it, until names were dropped from this entry point); if
;; two overlays can render different content under an equal cache-key,
;; namespace the key again.
(define *box-memo* '())

(define (%box region comp key)
  (let* ([avail-w (area-width region)]
         [hit (and (pair? *box-memo*)
               (equal? key (car (car *box-memo*)))
               (= avail-w (cadr (car *box-memo*))))]
         [nat (if hit (caddr (car *box-memo*))
               (begin
                 ;; cache miss: measure and store
                 (let ([n (and comp (native-component-required-size
                                     comp
                                     (list avail-w (area-height region))))])
                   (set! *box-memo* (cons (list key avail-w n) *box-memo*))
                   n)))])
    (let* ([rw avail-w]
           [rh (area-height region)]
           [w (if (pair? nat) (min (max 1 (car nat)) rw) rw)]
           [h (if (pair? nat) (min (max 1 (cadr nat)) rh) rh)]
           [x (+ (area-x region) (quotient (- rw w) 2))]
           [y (+ (area-y region) (quotient (- rh h) 2))])
      (area x y w h))))

;;@doc
;; Render a native component into a content-sized box centred in `region`.
;;
;; ```scheme
;; (split-overlay-render-native! buffer region content [opaque? #t]) -> void?
;; ```
;;;; #### Arguments
;; - `buffer` (`Buffer?`) - target buffer to render into.
;; - `region` (`Rect?`) - available region (from `split-overlay-region`).
;; - `content` (`(-> (or/c #f (cons/c Component? any/c)))`) - callback returning `#f` when
;;   there's nothing to show, else `(cons component cache-key)` where `cache-key`
;;   changes whenever the content does.
;; - `opaque?` (`boolean?`) - when `#t` (default), clear the box with `split-overlay-bg-style`
;;   before rendering; when `#f`, render transparently.
;;
;; #### Returns
;; `void?`
;;
;; #### Examples
;; ```scheme
;; (define (render-fn state rect buf)
;;   (split-overlay-render-native! buf rect
;;     (lambda ()
;;       (if (hash-has-key? state 'items)
;;         (cons (markdown-component (hash-ref state 'items)) (hash-ref state 'version))
;;         #f))))
;; ```
(define (split-overlay-render-native! buffer region content [opaque? #t])
  (let ([c (content)])
    (when c
      (let* ([comp (car c)]
             [key (cdr c)]
             [box (%box region comp key)])
        (when opaque? (buffer/clear-with buffer box split-overlay-bg-style))
        (render-native-component comp box buffer)))))
