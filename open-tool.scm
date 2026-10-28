;; open-tool.scm — run a program in a PTY, drawn inside the split it was opened from
;;
;; The engine is steel-pty (github.com/mattwparas/steel-pty): a real PTY, a VT
;; emulator, and its cell painter. This file adds placement and lifecycle from
;; split-overlay.scm: the overlay sits over the anchor split's rect (no floating
;; box, no editor clip), so it stays over the split you launched it from and
;; follows that split around.
;;
;; Every key goes to the program — helix's keymap and `:` prompt are unreachable
;; until the program exits. The overlay closes when the program exits,
;; or when its split/document goes away; closing the overlay kills the child.
;;
;; NOTE: Borrowed from steel-pty's term.scm: colour translation
;; (%cell-style!/%attribute->color, term.scm:102-138), the paint loop
;; (terminal-render, term.scm:459-523), and the key table
;; (terminal-event-handler, term.scm:624-718). Omitted: floating box,
;; set-editor-clip-right!, mouse handling, ctrl-l/ctrl-esc/shift-tab special
;; keys, and *command-terminal* globals.

(require "helix/misc.scm") ; set-status!, helix-await-callback
(require "helix/components.scm") ; style/Color/frame-set-string!/events
(require "helix/static.scm") ; get-helix-cwd
(require "split-overlay.scm") ; open!/close! overlap, region, bg-style
(require-builtin steel/ffi) ; ffi-vector, ffi-vector-ref, mutable-string
(require-builtin steel/filesystem) ; path-exists?

(#%require-dylib "libsteel_wezterm"
  (only-in create-native-pty-system-with-args-and-cwd!
    kill-pty-process!
    pty-process-send-command
    pty-process-send-command-char
    async-try-read-line
    virtual-terminal
    vte/advance-bytes
    vte/resize
    pty-resize!
    vte/reset-iterator!
    vte/advance-iterator-and-update-cells!
    vte/iter-x
    vte/iter-y
    vte/iter-cell-reverse?
    vte/empty-cell
    term/color-attribute-set!
    vte/cursor-x
    vte/cursor-y
    vte/cursor-visible?))

(provide open-tool close-tool)

;; ------------------------------------------------------------------ state

;; kill-switch mirrors steel-pty's Terminal-kill-switch: checked first in the
;; event handler, returns event-result/close for synchronous compositor removal.
;; The job-based pop-last-component-by-name! is best-effort; this path is the
;; authoritative close when the child exits or the user forces it.
(struct Tool (name vte pty rows cols str cell-fg cell-bg kill-switch))

;; Reused scratch: one string and two cell attributes per overlay, plus the two
;; rgb vectors and colors the style translation needs. The dylib overwrites
;; them per cell, so nothing allocates in the paint loop.
(define bg-attr (ffi-vector #f #f #f #f))
(define fg-attr (ffi-vector #f #f #f #f))
(define fg-scratch (Color/rgb 0 0 0))
(define bg-scratch (Color/rgb 0 0 0))
;; A cell with no colour uses the theme's text colour; otherwise the emulator's
;; default cells would inherit the previously written color.
(define theme-fg (or (style->fg (theme-scope "ui.text")) Color/White))

;; ------------------------------------------------------------------ paint helpers
;; Defined early: used inside `while` in %render!, and macros expand their body
;; at the call site.

;; Cell attribute -> color, borrowed from term.scm. `attr` is #t (truecolour,
;; rgb already written into the shared vector), an int (palette index), or #f
;; (the terminal default).
(define (%attribute->color attr attr-vec scratch fg?)
  (cond
    [(int? attr)
      (set-color-indexed! scratch attr)
      scratch]
    [attr
      (set-color-rgb! scratch
        (ffi-vector-ref attr-vec 0)
        (ffi-vector-ref attr-vec 1)
        (ffi-vector-ref attr-vec 2))
      scratch]
    ;; default background is Color/Reset, not the scratch color, so a default
    ;; cell never inherits the previous cell's background
    [else (if fg? theme-fg Color/Reset)]))

(define (%cell-style! style fg bg reverse?)
  (define bgc (or (%attribute->color (term/color-attribute-set! bg bg-attr) bg-attr bg-scratch #f)
               Color/Reset))
  (define fgc (or (%attribute->color (term/color-attribute-set! fg fg-attr) fg-attr fg-scratch #t)
               Color/Reset))
  (if reverse?
    (begin (set-style-bg! style fgc) (set-style-fg! style bgc))
    (begin (set-style-bg! style bgc) (set-style-fg! style fgc))))

;; ------------------------------------------------------------------ pre-flight

;; Check that a program is resolvable before spawning: steel-pty's
;; `spawn_command(cmd).unwrap()` panics on a missing binary, which aborts hx
;; instead of reporting an error. The check must happen in Scheme.
;; A name containing '/' is a path; otherwise it must be found on $PATH.
(define (executable-exists? prog)
  (cond
    [(string-contains? prog "/")
      (path-exists? prog)]
    [else
      (let ([path (env-var "PATH")])
        (if (not path)
          #f
          (let loop ([rest (string->list path)] [acc ""])
            (cond
              [(null? rest)
                (path-exists? (string-append acc "/" prog))]
              [(char=? (car rest) #\:)
                (if (path-exists? (string-append acc "/" prog))
                  #t
                  (loop (cdr rest) ""))]
              [else
                (loop (cdr rest) (string-append acc (string (car rest))))]))))]))

;; ------------------------------------------------------------------ pump

;; Reads the PTY asynchronously and feeds the emulator. The dylib returns #f
;; only when the child is gone, so EOF means the program exited: arm the
;; kill-switch (so the next event closes the component synchronously), then
;; run the async pop + on-close for redundancy.
;; Defined before open-tool because open-tool's body references it directly and
;; Steel resolves identifiers at expansion time.
(define (%pump state)
  (helix-await-callback (async-try-read-line (Tool-pty state))
    (lambda (line)
      (cond
        [(not line)
          (set-box! (Tool-kill-switch state) #t)
          (split-overlay-close! "open-tool")]
        [else
          (vte/advance-bytes (Tool-vte state) line)
          (%pump state)]))))

;; ------------------------------------------------------------------ paint

;; The emulator and PTY match the box size: SIGWINCH makes the program repaint
;; at the new size. Runs before painting so this frame sees the resized grid.
(define (%sync-size! state region)
  (let* ([w (area-width region)]
         [h (area-height region)])
    (unless (and (= w (unbox (Tool-cols state))) (= h (unbox (Tool-rows state))))
      (set-box! (Tool-cols state) w)
      (set-box! (Tool-rows state) h)
      (vte/resize (Tool-vte state) h w)
      (pty-resize! (Tool-pty state) h w))))

(define (%render! state rect buffer)
  (let* ([region (split-overlay-region (Tool-name state) rect)]
         [x0 (area-x region)]
         [y0 (area-y region)]
         [style (style)]
         [vte (Tool-vte state)])
    (%sync-size! state region)
    (buffer/clear-with buffer region split-overlay-bg-style)
    (vte/reset-iterator! vte)
    (while (vte/advance-iterator-and-update-cells! vte
            (Tool-str state)
            (Tool-cell-bg state)
            (Tool-cell-fg state))
      (%cell-style! style (Tool-cell-fg state) (Tool-cell-bg state)
        (vte/iter-cell-reverse? vte))
      (frame-set-string! buffer
        (+ x0 (vte/iter-x vte))
        (+ y0 (vte/iter-y vte))
        (Tool-str state)
        style))))

(define (%cursor! state rect)
  (let ([region (split-overlay-region (Tool-name state) rect)])
    (if (vte/cursor-visible? (Tool-vte state))
      ;; column offset unverified against a real renderer —
      ;; steel-pty adds +1 for its own border (term.scm:523); check first
      ;; live with a TUI that shows a caret (lazygit's commit box)
      (list (position (+ (area-y region) (vte/cursor-y (Tool-vte state)))
             (+ (area-x region) (vte/cursor-x (Tool-vte state))))
        "block")
      (list #f "hidden"))))

;; ------------------------------------------------------------------ input

;; Everything is forwarded: no chord closes the overlay, and the editor gets
;; no key while a program runs. Mouse consumption drops clicks instead of
;; forwarding them; wiring the XTerm mouse protocol through to the PTY is the
;; upgrade when a program needs it.
;; Everything is forwarded to the program ONLY when the anchor split is focused.
;; When focus moves to another split, events fall through to helix (scroll,
;; select, copy, command mode, etc.). The overlay stays rendered and the child
;; keeps running. Kill-switch (child exit) still closes unconditionally.
(define (%handle-event! state event)
  ;; kill-switch first: authoritative close on child exit, regardless of focus.
  (cond
    [(unbox (Tool-kill-switch state))
      (split-overlay-close! "open-tool")
      event-result/close]
    ;; When our anchor split is not focused, ignore events so helix handles them.
    [(not (equal? (editor-focus) (split-overlay-anchor-view "open-tool")))
      event-result/ignore]
    [else
      (let ([pty (Tool-pty state)])
        (define (send s) (pty-process-send-command pty s) event-result/consume)
        (define (send-char c) (pty-process-send-command-char pty c) event-result/consume)
        (cond
          [(paste-event? event)
            (let ([pasted (paste-event-string event)])
              (when (string? pasted)
                (pty-process-send-command pty pasted)))
            event-result/consume]
          [(key-event-backspace? event) (send "\x7f;")]
          [(key-event-escape? event) (send "\x1b;")]
          [(key-event-enter? event) (send "\r")]
          [(key-event-tab? event) (send "\t")]
          [(key-event-up? event) (send "\x1b;[A")]
          [(key-event-down? event) (send "\x1b;[B")]
          [(key-event-right? event) (send "\x1b;[C")]
          [(key-event-left? event) (send "\x1b;[D")]
          [(mouse-event? event) event-result/consume]
          [(not (key-event? event)) event-result/ignore]
          [(and (key-event? event) (equal? (key-event-modifier event) key-modifier-ctrl))
            (send-char (integer->char (bitwise-and (char->integer (key-event-char event)) #x1f)))]
          [(key-event-char event)
            (send-char (key-event-char event))]
          [else
            ;; Unforwardable keys (F-keys, Home/End, NUL...) close the overlay.
            ;; This is the force-quit for programs that never die. With upstream
            ;; libsteel_pty (no EOF) this was also the only escape from a stuck
            ;; overlay. Either way it closes and kills the child.
            (split-overlay-close! "open-tool")
            (set-status! "open-tool: closed by an unforwardable key")
            event-result/close]))]))

;; ------------------------------------------------------------------ command

;;@doc
;; Open a program in a PTY drawn inside the current split.
;;
;; ```scheme
;; (open-tool program . args) -> void?
;; ```
;;
;; Every key goes to the program until it exits. Helix keymap and `:` prompt
;; are unreachable while the program runs.
;;
;; #### Arguments
;; - `program` (`string?`) - executable name or path. If it contains `/`, treated
;;   as a path; otherwise searched on `$PATH`.
;; - `args` (`(listof string?)`) - arguments passed to the program.
;;
;; #### Returns
;; `void?`: opens the overlay asynchronously. Errors reported via status line.
;;
;; #### Examples
;; ```scheme
;; (open-tool "htop")              ;; => #<void>
;; (open-tool "lazygit")           ;; => #<void>
;; (open-tool "git" "log" "--oneline" "-20") ;; => #<void>
;; ```
(define (open-tool . args)
  (cond
    [(null? args)
      (set-status! "usage: :open-tool <program> [args...]")]
    [(split-overlay-open? "open-tool")
      (set-status! "a tool is already open")]
    [else
      (let* ([program (car args)]
             [pargs (cdr args)])
        (if (executable-exists? program)
          (let* ([pty (create-native-pty-system-with-args-and-cwd! program pargs (or (get-helix-cwd) "."))]
                 [vte (virtual-terminal pty)]
                 [tool (Tool "open-tool" vte pty (box 0) (box 0) (mutable-string)
                        (vte/empty-cell)
                        (vte/empty-cell)
                        (box #f))])
            (split-overlay-open!
              "open-tool"
              tool
              (lambda (state rect buffer) (%render! state rect buffer))
              (lambda (state event) (%handle-event! state event))
              (lambda (state rect) (%cursor! state rect))
              ;; killing the child ends the read loop, whichever path closed the
              ;; overlay; this thunk takes no arguments (close! calls it bare),
              ;; so it closes over tool rather than a callback parameter
              (lambda () (kill-pty-process! (Tool-pty tool))))
            (%pump tool))
          (set-status! (string-append "open-tool: command not found: " program))))]))

;;@doc
;; Close the running tool, if any.
;;
;; ```scheme
;; (close-tool) -> void?
;; ```
;;
;; #### Returns
;; `void?`: closes the overlay and kills the child process.
(define (close-tool)
  (split-overlay-close! "open-tool"))
