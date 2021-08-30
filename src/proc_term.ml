module Caml_unix = Unix

open Core_kernel

type t = {
  pid : int;
  pty : Pty.t;
  (*fd : Lwt_unix.file_descr;*)
  input : Lwt_io.input_channel;
  output : Lwt_io.output_channel;
  vterm : Vterm.t;
  stopped : Lwt_unix.process_status Lwt.t;
  on_damage : Vterm.Rect.t Listeners.t;
  on_move_cursor : (Vterm.Pos.t * Vterm.Pos.t * bool) Listeners.t;
  on_move_rect : (Vterm.Rect.t * Vterm.Rect.t) Listeners.t;
  on_term_prop : Vterm.TermProp.t Listeners.t;
}

let default_rows = 20
let default_cols = 50

let string_of_row (row : Vterm.ScreenCell.t array) =
  row
  |> Array.map ~f:(fun cell -> cell.char |> Uchar.to_char_exn)
  |> Array.to_list |> String.of_char_list

let run (cmd : Cmd.t) =
  let prog, args =
    match cmd.command with
    | Args (name, args) -> (name, args)
    | Shell cmd -> Lwt_process.shell cmd
  in
  let prog =
    if String.equal prog "" && Array.length args > 0 then args.(0) else prog
  in

  let vterm = Vterm.make ~rows:default_rows ~cols:default_cols in

  let pty =
    Pty.create ?env:cmd.env (prog, args) ~rows:default_rows ~cols:default_cols
  in

  let pid = Pty.get_pid pty in
  let fd = Pty.get_fd pty |> Lwt_unix.of_unix_file_descr in
  let input = Lwt_io.of_fd ~mode:Lwt_io.input fd in
  let output = Lwt_io.of_fd ~mode:Lwt_io.output fd in

  if Sys.win32 then [%log warn "Lwt_unix is about to be used on Windows."];
  let stopped = Lwt_unix.waitpid [] pid |> Lwt.map Tuple2.get2 in

  let (_ : unit Lwt.t) =
    Lwt_io.read_chars input
    |> Lwt_stream.iter (fun c ->
           let str = String.make 1 c in
           let (_ : int) = Vterm.write ~input:str vterm in
           ())
  in
  Vterm.setOutputCallback
    ~onOutput:(fun s -> Lwt_io.write output s |> (ignore : unit Lwt.t -> unit))
    vterm;

  let on_damage = Listeners.create () in
  Vterm.Screen.setDamageCallback
    ~onDamage:(fun rect -> Listeners.call on_damage rect)
    vterm;

  let on_move_cursor = Listeners.create () in
  Vterm.Screen.setMoveCursorCallback
    ~onMoveCursor:(fun pos old_pos visible ->
      Listeners.call on_move_cursor (pos, old_pos, visible))
    vterm;

  let on_move_rect = Listeners.create () in
  Vterm.Screen.setMoveRectCallback
    ~onMoveRect:(fun a b -> Listeners.call on_move_rect (a, b))
    vterm;

  let on_term_prop = Listeners.create () in
  Vterm.Screen.setTermPropCallback
    ~onSetTermProp:(fun prop -> Listeners.call on_term_prop prop)
    vterm;

  let sb_buffer : Vterm.sb_line list ref = ref [] in

  Vterm.Screen.setScrollbackPopCallback
    ~onPopLine:(fun () ->
      match !sb_buffer with
      | [] -> None
      | line :: rest ->
          sb_buffer := rest;
          Some line)
    vterm;

  Vterm.Screen.setScrollbackPushCallback
    ~onPushLine:(fun line -> sb_buffer := line :: !sb_buffer)
    vterm;

  (*Vterm.Screen.setAltScreen ~enabled:true vterm;*)
  Vterm.setUtf8 ~utf8:true vterm;

  {
    pid;
    pty;
    (*fd;*)
    input;
    output;
    vterm;
    stopped;
    on_damage;
    on_move_cursor;
    on_move_rect;
    on_term_prop;
  }

let resize ~rows ~cols pt = Pty.resize ~rows ~columns:cols pt.pty

let send_key pt (key : LTerm_key.t) =
  let modifier =
    if key.control then Vterm.Control
    else if key.meta then Vterm.Alt
    else if key.shift then Vterm.Shift
    else Vterm.None
  in

  let send key mod_ = Vterm.Keyboard.input pt.vterm key mod_ in

  match LTerm_key.code key with
  | Char c ->
      let uchar = c |> CamomileLibrary.UChar.uint_code |> Uchar.of_scalar_exn in
      send (Unicode uchar) modifier
  | Backspace -> send Vterm.Backspace modifier
  | Escape -> send Vterm.Escape modifier
  | Enter -> send Vterm.Enter modifier
  | Tab -> send Vterm.Tab modifier
  | Up -> send Vterm.Up modifier
  | Down -> send Vterm.Down modifier
  | Left -> send Vterm.Left modifier
  | Right -> send Vterm.Right modifier
  | Insert -> send Vterm.Insert modifier
  | Delete -> send Vterm.Delete modifier
  | Home -> send Vterm.Home modifier
  | End -> send Vterm.End modifier
  | Next_page -> send Vterm.PageDown modifier
  | Prev_page -> send Vterm.PageUp modifier
  | _ ->
      [%log warn "Proc_term.send_key ignored key: %s" (LTerm_key.to_string key)]
  (* To handle Uchar.of_scalar_exn *)
  | exception ex ->
      [%log warn "Proc_term.send_key error: %s" (Exn.to_string ex)]

let stop pt =
  let term_timer = Lwt_unix.sleep 5.0 in
  Lwt.on_success term_timer (fun () -> Caml_unix.kill pt.pid Sys.sigterm);
  let kill_timer = Lwt_unix.sleep 10. in
  Lwt.on_success kill_timer (fun () -> Caml_unix.kill pt.pid Sys.sigkill);

  Lwt.on_termination pt.stopped (fun _ ->
      Lwt.cancel term_timer;
      Lwt.cancel kill_timer);

  Vterm.Keyboard.input pt.vterm
    (Vterm.Unicode (Uchar.of_char 'c'))
    Vterm.Control

let stopped pt = pt.stopped