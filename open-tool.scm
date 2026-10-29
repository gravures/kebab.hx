;; open-tool.scm — run a program in a PTY, drawn inside the split it was opened from
;;
;; This file adds placement and lifecycle from split-overlay.scm:
;; the overlay sits over the anchor split's rect (no floating
;; box, no editor clip), so it stays over the split you launched
;; it from and follows that split around.
;;
;; Every key goes to the program while the tool holds focus. `ctrl-]` (see
;; *open-tool-release-chord* below) gives them back to helix: scroll, select,
;; copy, `:command` -- the overlay stays up, painting, and the child keeps
;; running. The overlay closes when the program exits, or when its
;; split/document goes away; closing the overlay kills the child.
;;
;; NOTE: colour translation, paint loop and key table are borrowed from
;; steel-pty's term.scm.

(require "helix/misc.scm")
(require "helix/components.scm")
(require "helix/static.scm")
(require "helix/editor.scm")
(require "helix/configuration.scm")
(require "split-overlay.scm")
(require-builtin steel/ffi)
(require-builtin steel/filesystem)

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
    vte/advance-run!
    vte/iter-run-x
    vte/iter-run-y
    vte/iter-cell-reverse?
    vte/empty-cell
    term/color-attribute-set!
    vte/cursor-x
    vte/cursor-y
    vte/cursor-visible?))

(provide open-tool close-tool *open-tool-release-chord*)

;; ------------------------------------------------------------------ config

;; Chord to hand keys back to helix. Format: (list modifier-flag char).
;; modifier-flag matches by BITMASK, so extra flags still match 'ctrl; combined
;; symbols are not supported. Char: #\w #\] #\Space.
;; Default: (ctrl #\]), the telnet escape char.
;; Custom: (set! *open-tool-release-chord* '(alt #\w))
(define *open-tool-release-chord* '(ctrl #\]))

;; Crossterm's legacy C0 parser (event/sys/unix/parse.rs) decodes a control byte
;; into a PRINTABLE char: 0x01..0x1A -> 'a'..'z', 0x1C..0x1F -> '4'..'7',
;; 0x00 -> ' '.
;; A direct comparison against #\] therefore never matches -- compare the alias
;; as well. Both spellings are accepted: a kitty-protocol terminal sends
;; CSI 93;5u, which crossterm decodes to the literal ']'.
(define (%crossterm-ctrl-alias ch)
  (cond
    [(char=? ch #\\) #\4]
    [(char=? ch #\]) #\5]
    [(char=? ch #\^) #\6]
    [(char=? ch #\_) #\7]
    [else #f]))

;; Inverse of the table above: the byte the terminal actually sent.
;; `char & 0x1f` alone sent ctrl-\ as byte 20, not 28.
(define (%crossterm-ctrl-byte ch)
  (let ([c (char->integer ch)])
    (cond
      [(and (>= c 97) (<= c 122)) (- c 96)] ; 'a'..'z' -> 0x01..0x1A
      [(and (>= c 52) (<= c 55)) (+ (- c 52) 28)] ; '4'..'7' -> 0x1C..0x1F
      [(and (>= c 123) (<= c 125)) (bitwise-and c #x1f)] ; { | } -> 0x1B 0x1C 0x1D
      [else (bitwise-and c #x1f)]))) ; ' ' -> 0x00, and the rest

;; Check if event matches the configured release chord.
(define (%matches-release-chord? event)
  (and (key-event? event)
    (let* ([chord *open-tool-release-chord*]
           [mod (car chord)]
           [ch (cadr chord)]
           [mod-flag (cond
                      [(equal? mod 'ctrl) key-modifier-ctrl]
                      [(equal? mod 'alt) key-modifier-alt]
                      [(equal? mod 'shift) key-modifier-shift]
                      [else 0])])
      (and (not (= mod-flag 0))
        (not (= (bitwise-and (key-event-modifier event) mod-flag) 0))
        (let ([reported (key-event-char event)]
              [alias (%crossterm-ctrl-alias ch)])
          (or (equal? reported ch)
            (and alias (equal? reported alias))))))))

;; ------------------------------------------------------------------ mode indicator
;;
;; The statusline draws one mode word, only on the focused view
;; (statusline.rs:196), read from config `statusline.mode.normal`. While the
;; tool holds the keys the word reads TOOL and the buffer is forced to Normal
;; mode, so the word always means "helix has the keys". Releasing restores the
;; saved word, re-acquiring sets TOOL again. The save reads live config, so a
;; restore never overwrites the word the user configured.

;; The user's normal-mode word, or #f while the indicator is off.
(define *mode-normal-saved* #f)

;; Show the indicator: force Normal mode, then relabel the statusline word.
(define (%tool-mode-on!)
  (editor-set-mode! (string->editor-mode "normal"))
  (when (not *mode-normal-saved*)
    (let ([cur (get-config-option-value "statusline.mode.normal")])
      (set! *mode-normal-saved* (if (string? cur) cur "NOR"))))
  (set-option! "statusline.mode.normal" "TOOL")
  (update-configuration!))

;; Put the user's word back. Idempotent: a second close restores nothing.
(define (%tool-mode-off!)
  (when *mode-normal-saved*
    (set-option! "statusline.mode.normal" *mode-normal-saved*)
    (update-configuration!)
    (set! *mode-normal-saved* #f)))

;; ------------------------------------------------------------------ state

;; kill-switch mirrors steel-pty's Terminal-kill-switch: checked first in the
;; event handler, returns event-result/close for synchronous compositor removal.
;; The job-based pop-last-component-by-name! is best-effort; this path is the
;; authoritative close when the child exits or the user forces it.
;;
;; released-to-helix: #t once the user pressed the release chord, or once focus
;; moved off the anchor view. In this mode ALL keys go to helix, the child keeps
;; running, and the overlay keeps painting. It only clears when the chord is
;; pressed again -- no automatic re-acquire, since a focus move onto the anchor
;; view is not the same gesture as asking for the keys back.
;;
;; While released the region comes from split-overlay's cache branch, because
;; the focus branch is no longer readable: a terminal resize in this mode is
;; picked up on the next refocus.

;; NOTE: there is no view-exists API to tell "anchor blurred" from "anchor
;; closed". An anchor split closed while released leaves a frozen, unreachable
;; tool, and no path in this file detects that case.
(struct Tool (name vte pty rows cols str cell-fg cell-bg kill-switch released-to-helix region))

;; Reused scratch: one string and two cell attributes per overlay, plus the two
;; rgb vectors and colors the style translation needs. The dylib overwrites
;; them per style-run, so nothing allocates in the paint loop.
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
;; rgb already written into the shared vector), an int (palette index),
;; or #f (the terminal default).
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
    ;; default background is Color/Reset, so a default
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
;; `spawn_command(cmd).unwrap()` panics on a missing binary, which aborts
;; hx instead of reporting an error. The check must happen in Scheme.
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

;; Reads the PTY asynchronously and feeds the emulator. The dylib returns
;; #f only when the child is gone, so EOF means the program exited: arm
;; the kill-switch (so the next event closes the component synchronously),
;; then run the async pop + on-close for redundancy.
;; Defined before open-tool because open-tool's body references it directly
;; and Steel resolves identifiers at expansion time.
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
  (set-box! (Tool-region state) region)
  (let* ([w (area-width region)]
         [h (area-height region)])
    (unless (and (= w (unbox (Tool-cols state))) (= h (unbox (Tool-rows state))))
      (set-box! (Tool-cols state) w)
      (set-box! (Tool-rows state) h)
      (vte/resize (Tool-vte state) h w)
      (pty-resize! (Tool-pty state) h w))))

;; Paint by style-run (cells sharing fg/bg/reverse): one frame-set-string! per
;; run keeps a full repaint cheap. The run start is the cell's own column.
(define (%render! state rect buffer)
  (let* ([region (split-overlay-region (Tool-name state) rect)]
         [x0 (area-x region)]
         [y0 (area-y region)]
         [style (style)]
         [vte (Tool-vte state)])
    (%sync-size! state region)
    (buffer/clear-with buffer region split-overlay-bg-style)
    (vte/reset-iterator! vte)
    (while (vte/advance-run! vte
            (Tool-str state)
            (Tool-cell-bg state)
            (Tool-cell-fg state))
      (%cell-style! style (Tool-cell-fg state) (Tool-cell-bg state)
        (vte/iter-cell-reverse? vte))
      (frame-set-string! buffer
        (+ x0 (vte/iter-run-x vte))
        (+ y0 (vte/iter-run-y vte))
        (Tool-str state)
        style))))

;; While released, report no cursor: Compositor::cursor takes the topmost
;; layer's cursor and stops, so the tool's caret would mask helix's own in the
;; split you work in.
(define (%cursor! state rect)
  (if (unbox (Tool-released-to-helix state))
    (list #f "hidden")
    (let ([region (split-overlay-region (Tool-name state) rect)])
      (if (vte/cursor-visible? (Tool-vte state))
        (list (position (+ (area-y region) (vte/cursor-y (Tool-vte state)))
               (+ (area-x region) (vte/cursor-x (Tool-vte state))))
          "block")
        (list #f "hidden")))))

;; ------------------------------------------------------------------ input

;; Press/release and scroll, re-encoded as SGR reports on the child's grid:
;; the region origin shifts the screen coordinates into the overlay's own space,
;; where the child believes its window starts at (1,1). Drag and plain motion
;; are dropped -- no tool here needs them, and htop-style TUIs that enable
;; mouse reporting themselves never miss them.
;; No paint per mouse event: helix repaints the whole compositor per consumed
;; event (~17 ms), and the child's output triggers a repaint when it arrives.
(define (%forward-mouse! state event)
  (let* ([region (unbox (Tool-region state))]
         [cx (event-mouse-col event)]
         [cy (event-mouse-row event)]
         [x (if region (area-x region) 0)]
         [y (if region (area-y region) 0)]
         [w (if region (area-width region) 0)]
         [h (if region (area-height region) 0)])
    (define (inside?)
      (and (>= cx x) (< cx (+ x w)) (>= cy y) (< cy (+ y h))))
    (cond
      ;; outside the overlay: helix keeps it (focus another split, select text)
      [(not (inside?)) event-result/ignore]
      [else
        (let* ([kind (event-mouse-kind event)]
               [button (cond [(= kind 0) 0] ; left
                        [(= kind 1) 2] ; right
                        [(= kind 2) 1] ; middle
                        [(= kind 3) 0]
                        [(= kind 4) 2]
                        [(= kind 5) 1]
                        [(= kind 10) 65] ; wheel down
                        [(= kind 11) 64] ; wheel up
                        [(= kind 12) 66] ; wheel left
                        [(= kind 13) 67] ; wheel right
                        [else #f])]
               ;; release reports the same button with a lowercase terminator
               [end (if (and (>= kind 3) (<= kind 5)) "m" "M")])
          (if (not button)
            ;; drag/motion: swallowed,
            ;; never forwarded event-result/consume-without-rerender
            (begin
              (pty-process-send-command (Tool-pty state)
                (string-append "\x1b;[<" (number->string button) ";"
                  (number->string (+ 1 (- cx x)))
                  ";"
                  (number->string (+ 1 (- cy y)))
                  end))
              event-result/consume-without-rerender)))])))

;; Keys go to the program ONLY while the tool has focus: the release chord gives
;; them back to helix (scroll, select, copy, command mode, the `:` prompt) while
;; the overlay stays rendered and the child keeps running.
;; Kill-switch (child exit) still closes unconditionally, whichever mode is in.
;; Mouse press/release and wheel go to the program; drag is swallowed.
;;
;; NOTE: alt loses its modifier on non-char keys (key-event-up? and friends match
;; the key code only, components.rs:714).
(define (%handle-event! state event)
  (cond
    [(unbox (Tool-kill-switch state))
      (split-overlay-close! "open-tool")
      event-result/close]

    [(%matches-release-chord? event)
      (let ([released? (not (unbox (Tool-released-to-helix state)))])
        (set-box! (Tool-released-to-helix state) released?)
        (if released? (%tool-mode-off!) (%tool-mode-on!)))
      ;; consume so this will not bubble the chord into helix's own keymap
      event-result/consume]

    ;; Auto-release when focus leaves anchor split.
    [(not (equal? (editor-focus) (split-overlay-anchor-view "open-tool")))
      (if (unbox (Tool-released-to-helix state))
        event-result/ignore
        (begin
          (set-box! (Tool-released-to-helix state) #t)
          (%tool-mode-off!)
          event-result/ignore))]

    ;; In released mode: all keys go to helix.
    [(unbox (Tool-released-to-helix state))
      event-result/ignore]

    ;; Forward to PTY when anchor split is focused and not released.
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

          ;; Steel hex escapes are terminated by ';' (\x1b;
          [(key-event-up? event) (send "\x1b;[A")]
          [(key-event-down? event) (send "\x1b;[B")]
          [(key-event-right? event) (send "\x1b;[C")]
          [(key-event-left? event) (send "\x1b;[D")]
          [(mouse-event? event) (%forward-mouse! state event)]
          [(not (key-event? event)) event-result/ignore]

          ;; ctrl-chars: decode the byte the terminal actually sent via the
          ;; crossterm table above, and a charless ctrl-key (ctrl+F1) falls
          ;; through to the close below rather than erroring on char->integer
          [(and (key-event? event)
              (key-event-char event)
              (not (= (bitwise-and (key-event-modifier event) key-modifier-ctrl) 0)))
            (send-char (integer->char (%crossterm-ctrl-byte (key-event-char event))))]
          [(and (key-event-char event)
              (not (= (bitwise-and (key-event-modifier event) key-modifier-alt) 0)))
            (send (string-append "\x1b;" (string (key-event-char event))))]
          [(key-event-char event)
            (send-char (key-event-char event))]
          [else
            ;; Unforwardable keys (F-keys, Home/End, NUL...) close the overlay.
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
;; Every key goes to the program until the release chord (default `ctrl-]`)
;; is pressed or focus moves to another split; the overlay stays up and the
;; child keeps running, so the program is resumed by pressing the chord again.
;;
;; Mouse press/release and wheel go to the program; drag does not.
;; An unforwardable key (F-keys, Home/End) closes the overlay.
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
                        (box #f)
                        (box #f)
                        (box #f))])
            (split-overlay-open!
              "open-tool"
              tool
              (lambda (state rect buffer) (%render! state rect buffer))
              (lambda (state event) (%handle-event! state event))
              (lambda (state rect) (%cursor! state rect))

              ;; Single close hook: every path ends here (pump EOF, unforwardable
              ;; key, close-tool, kill-switch, split-overlay's anchor-gone hook,
              ;; which bypasses %handle-event!). Restoring the word here keeps
              ;; TOOL from surviving child exit; killing the child ends the read
              ;; loop. close! calls the thunk bare, so it closes over tool.
              (lambda ()
                (%tool-mode-off!)
                (kill-pty-process! (Tool-pty tool))))
            (%tool-mode-on!)
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
