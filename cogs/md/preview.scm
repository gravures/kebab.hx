;; md-preview.scm — markdown preview overlay for Steel Helix
;;
;; Usage: require in init.scm, then :md-preview-toggle or SPC m p
;;
;; Placement, sizing and the component lifecycle live in split-overlay.scm;
;; this file only knows about markdown: which document to read, and how to
;; cache it. Closing the split, or the document behind the preview, is the
;; overlay's own view-anchor; this file only adds Esc.

(require "helix/editor.scm")
(require "helix/components.scm")
(require "helix/misc.scm")
(require-builtin helix/core/text)
(require "kebab/overlay/split-overlay.scm")

(provide md-preview-toggle md-preview-open md-preview-close)

;; Last parsed markdown, so rendering only re-parses when the source changed.
;; The key doubles as the box memo's cache key: it changes exactly when the
;; text does, so a resize re-asks the size without re-parsing.
(define *md-preview-src* "")
(define *md-preview-comp* #f)

;;@doc
;; Open a markdown preview of the current buffer over the current split
;;
;; ```scheme
;; (md-preview-open) -> void?
;; ```
(define (md-preview-open)
  (unless (split-overlay-open? "md-preview")
    (let ([doc-id (editor->doc-id (editor-focus))])
      (split-overlay-open! "md-preview"
        doc-id ; state = source doc id
        (lambda (state area buffer) ; render: read the buffer live
          (split-overlay-render-native!
            buffer
            (split-overlay-region "md-preview" area)
            (lambda ()
              (let ([text (editor->text state)])
                (if text
                  (let ([src (rope->string text)])
                    (if (equal? src *md-preview-src*)
                      (cons *md-preview-comp* src)
                      (let ([comp (markdown-component src)])
                        (set! *md-preview-src* src)
                        (set! *md-preview-comp* comp)
                        (cons comp src))))
                  #f)))))
        (lambda (state event) ; Esc: close, flag and all
          (if (and (key-event? event) (key-event-escape? event))
            (begin
              (split-overlay-close! "md-preview")
              event-result/close)
            event-result/ignore)))
      (set-status! "markdown preview opened (Esc to close)"))))

;;@doc
;; Close the markdown preview
;;
;; ```scheme
;; (md-preview-close) -> void?
;; ```
(define (md-preview-close)
  (split-overlay-close! "md-preview"))

;;@doc
;; Toggle the markdown preview
;;
;; ```scheme
;; (md-preview-toggle) -> boolean?
;; ```
(define (md-preview-toggle)
  (split-overlay-toggle! "md-preview" md-preview-open md-preview-close))
