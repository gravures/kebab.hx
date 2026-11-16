;; git-blame.scm

(require "helix/editor.scm")
(require "steel/result")
(require "helix/configuration.scm")
(require (prefix-in helix. "helix/commands.scm"))
(require (prefix-in helix.static. "helix/static.scm"))
(require (only-in "helix/misc.scm"
          add-inlay-hint
          remove-inlay-hint-by-id))
(require-builtin helix/core/text)

(provide git-blame git-blame-hint)

;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;
;; Internal Functions

;; Get the file path of the focused document.
(define (%current-path)
  (let* ([focus (editor-focus)]
         [focus-doc-id (editor->doc-id focus)])
    (editor-document->path focus-doc-id)))

;; Compact "sha author date" from one `git blame --date=short` line.
;; assumes author name has no ")" in it.
(define (%blame->hint s)
  (let* ([inside (car (split-once (cadr (split-once s "(")) ")"))]
         [tokens (reverse (cdr (reverse (split-whitespace inside))))] ; drop trailing line no
         [sha (car (split-whitespace s))]
         [short-sha (if (starts-with? sha "^")
                     (substring sha 1 (min (string-length sha) 8))
                     (substring sha 0 (min (string-length sha) 7)))])
    (string-append short-sha " " (string-join tokens " "))))

;; Returns: Option<String> raw git blame line | None
(define (%git-blame-line line)
  (let ([range (string-append (number->string line) "," (number->string line))])
    (let ([out (unwrap-or
                (ok-and-then
                  (spawn-process
                    (~> (command "git" (list "blame" "-L" range (%current-path)))
                      with-stdout-piped
                      with-stderr-piped)) ; capture stderr to avoid UI corruption
                  wait->stdout)
                "")])
      ; wait->stdout returns (Ok "") even when git fails
      ; — exit code is never surfaced, so empty = None
      (if (equal? out "")
        (None)
        (Some out)))))

;; Last hint snapshot id returned by add-inlay-hint,
;; so a re-blame clears the old one.
(define %last-blame-hint-id (box #f))

;; Required for git-blame-hint: helix resets *all* inlay hints (including plugin
;; ones added via add-inlay-hint) on refresh when this is off.
(lsp (hash 'display-inlay-hints #t))

;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;
;; Typable Commands

;;@doc
;; Blame the current line
(define (git-blame)
  (let* ([line (helix.static.get-current-line-number)] ; 0 based
         [res (%git-blame-line (+ 1 line))])
    (if (Some? res)
      (helix.echo (%blame->hint (Some->value res)))
      (begin)))) ; no-op

;;@doc
;; Blame the current line as inlay hint
(define (git-blame-hint)
  (let* ([line (helix.static.get-current-line-number)] ; 0 based
         [res (%git-blame-line (+ 1 line))])
    (if (Some? res)
      (let* ([rope (editor->text (editor->doc-id (editor-focus)))]
             ; inline hint renders *before* the char at this index,
             ; so point at the newline = end of the visible line.
             [char-index (- (rope-line->char rope (+ 1 line)) 1)])
        (when (unbox %last-blame-hint-id)
          (apply remove-inlay-hint-by-id (unbox %last-blame-hint-id)))
        (set-box! %last-blame-hint-id
          (add-inlay-hint char-index (string-append " "
                                      (%blame->hint (Some->value res))))))
      (begin)))) ; no-op
