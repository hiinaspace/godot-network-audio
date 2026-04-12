extends Node

const SAMPLE_RATE := 48_000
const FRAME_SAMPLES := 960
const FRAME_SECONDS := float(FRAME_SAMPLES) / float(SAMPLE_RATE)
const INPUT_MODE_MICROPHONE := "microphone"
const INPUT_MODE_SYNTHETIC := "synthetic"
const STARTUP_PREBUFFER_PACKETS := 4
const DEFAULT_QUIT_SECONDS := 4.0

var sender
var stream
var player: AudioStreamPlayer
var stats_timer: Timer
var quit_timer: Timer

var phase := 0.0
var total_generated_frames := 0
var send_accumulator := 0.0
var input_mode := INPUT_MODE_MICROPHONE
var synthetic_fallback_seconds := 0.75
var demo_start_msec := 0
var startup_prebuffer_packets := STARTUP_PREBUFFER_PACKETS
var selected_input_device := ""
var selected_output_device := ""
var allow_synthetic_fallback := true
var force_synthetic := false
var quit_after_seconds := DEFAULT_QUIT_SECONDS
var trace_jsonl_path := ""
var trace_file: FileAccess = null
var trace_frame := 0
var enable_dtx := true


func _ready() -> void:
	demo_start_msec = Time.get_ticks_msec()
	_load_env_config()
	_open_trace_file()
	print("demo: ready")
	print("demo: class exists sender=", ClassDB.class_exists("NetworkAudioSender"))
	print("demo: class exists stream=", ClassDB.class_exists("AudioStreamNetwork"))
	print("demo: current output device=", AudioServer.output_device)
	print("demo: output devices=", AudioServer.get_output_device_list())
	print("demo: current input device=", AudioServer.input_device)
	print("demo: input devices=", AudioServer.get_input_device_list())
	_select_output_device()
	_select_input_device()

	sender = ClassDB.instantiate("NetworkAudioSender")
	stream = ClassDB.instantiate("AudioStreamNetwork")
	if sender == null or stream == null:
		push_error("demo: failed to instantiate extension classes")
		return

	sender.name = "Sender"
	sender.input_sample_rate_hz = int(AudioServer.get_mix_rate())
	sender.enable_dtx = enable_dtx
	sender.capture_audio_server_input = not force_synthetic
	sender.microphone_frame_budget = FRAME_SAMPLES
	add_child(sender)
	if force_synthetic:
		input_mode = INPUT_MODE_SYNTHETIC
		print("demo: forced synthetic input mode")

	player = AudioStreamPlayer.new()
	player.name = "Player"
	player.stream = stream
	player.autoplay = false
	add_child(player)

	if sender.has_method("connect_loopback_stream"):
		sender.connect_loopback_stream(stream)
	else:
		sender.packet_ready.connect(_on_packet_ready)
	sender.encoder_error.connect(_on_encoder_error)

	stats_timer = Timer.new()
	stats_timer.name = "StatsTimer"
	stats_timer.wait_time = 1.0
	stats_timer.one_shot = false
	stats_timer.autostart = false
	stats_timer.timeout.connect(_print_stats)
	add_child(stats_timer)

	quit_timer = Timer.new()
	quit_timer.name = "QuitTimer"
	quit_timer.wait_time = quit_after_seconds
	quit_timer.one_shot = true
	quit_timer.autostart = false
	quit_timer.timeout.connect(_shutdown_demo)
	add_child(quit_timer)

	stats_timer.start()
	quit_timer.start()

	print("demo: playback started, input_mode=", input_mode)


func _process(delta: float) -> void:
	if input_mode == INPUT_MODE_MICROPHONE:
		var elapsed_msec := Time.get_ticks_msec() - demo_start_msec
		if allow_synthetic_fallback and sender.get_captured_input_frames() <= 0 and elapsed_msec >= int(synthetic_fallback_seconds * 1000.0):
			sender.capture_audio_server_input = false
			input_mode = INPUT_MODE_SYNTHETIC
			print("demo: microphone fallback -> synthetic")
	else:
		send_accumulator += delta
		while send_accumulator >= FRAME_SECONDS:
			send_accumulator -= FRAME_SECONDS
			_push_synthetic_frame()

	_write_trace_row(delta)


func _push_synthetic_frame() -> void:
	var samples := PackedFloat32Array()
	samples.resize(FRAME_SAMPLES)

	for i in FRAME_SAMPLES:
		var t := phase + float(i) / float(SAMPLE_RATE)
		var carrier := sin(TAU * 220.0 * t)
		var overtone := 0.35 * sin(TAU * 440.0 * t)
		var wobble := 0.12 * sin(TAU * 3.0 * t)
		samples[i] = (carrier + overtone) * (0.18 + wobble)

	phase += FRAME_SECONDS
	_push_samples(samples)


func _push_samples(samples: PackedFloat32Array) -> void:
	var emitted: int = sender.push_pcm_mono(samples)
	total_generated_frames += 1
	if total_generated_frames <= 3:
		print("demo: pushed samples=", samples.size(), " emitted packets=", emitted)


func _on_packet_ready(bytes: PackedByteArray) -> void:
	var ok: bool = stream.push_packet(bytes)
	if not ok:
		push_error("demo: stream rejected packet bytes")
		return
	_maybe_start_playback()


func _on_encoder_error(message: String) -> void:
	push_error("demo: encoder error: %s" % message)


func _print_stats() -> void:
	var stats: Dictionary = stream.get_stats()
	var sender_stats: Dictionary = sender.get_stats()
	print(
		"demo: stats receiver=",
		JSON.stringify(stats),
		" sender=",
		JSON.stringify(sender_stats),
		" input_mode=",
		input_mode
	)


func _shutdown_demo() -> void:
	print("demo: shutting down")
	_close_trace_file()
	if is_instance_valid(stats_timer):
		stats_timer.stop()
	# Disconnect before stopping playback so no in-flight packet_ready fires
	# into a partially-torn-down stream.
	if is_instance_valid(sender) and sender.has_method("disconnect_loopback_stream"):
		sender.disconnect_loopback_stream()
	elif is_instance_valid(sender):
		sender.packet_ready.disconnect(_on_packet_ready)
	if is_instance_valid(player):
		player.stop()
		# Null the stream reference so the audio thread stops calling _mix()
		# on the playback object before the tree frees everything.
		player.stream = null
	# Release the GDScript-side stream ref so the RefCounted object reaches
	# zero before the tree does its ObjectDB cleanup.
	stream = null
	get_tree().quit()


func _open_trace_file() -> void:
	if trace_jsonl_path == "":
		return
	trace_file = FileAccess.open(trace_jsonl_path, FileAccess.WRITE)
	if trace_file == null:
		push_error("demo: failed to open trace file: %s" % trace_jsonl_path)
		return
	print("demo: trace jsonl=", trace_jsonl_path)


func _close_trace_file() -> void:
	if trace_file != null:
		trace_file.flush()
		trace_file = null


func _write_trace_row(delta: float) -> void:
	if trace_file == null or sender == null or stream == null:
		return
	var receiver_stats: Dictionary = stream.get_stats()
	var sender_stats: Dictionary = sender.get_stats()
	var row := {
		"frame": trace_frame,
		"mono_usec": Time.get_ticks_usec(),
		"delta_sec": delta,
		"input_mode": input_mode,
		"receiver": receiver_stats,
		"sender": sender_stats,
	}
	trace_file.store_line(JSON.stringify(row))
	trace_frame += 1


func _select_input_device() -> void:
	var current := String(AudioServer.input_device)
	var devices := AudioServer.get_input_device_list()
	if selected_input_device != "" and devices.has(selected_input_device):
		AudioServer.input_device = selected_input_device
		print("demo: selected input device=", selected_input_device)
		return
	if current != "" and devices.has(current):
		print("demo: keeping input device=", current)
		return

	for device in devices:
		var name := String(device)
		if name != "" and name != "Default":
			AudioServer.input_device = name
			print("demo: selected input device=", name)
			return

	if devices.has("Default"):
		AudioServer.input_device = "Default"
		print("demo: selected input device=Default")
	else:
		print("demo: no usable input device found")


func _select_output_device() -> void:
	var current := String(AudioServer.output_device)
	var devices := AudioServer.get_output_device_list()
	if selected_output_device != "" and devices.has(selected_output_device):
		AudioServer.output_device = selected_output_device
		print("demo: selected output device=", selected_output_device)
		return
	if current != "" and devices.has(current):
		print("demo: keeping output device=", current)
		return
	if devices.has("Default"):
		AudioServer.output_device = "Default"
		print("demo: selected output device=Default")
	else:
		print("demo: no usable output device found")


func _maybe_start_playback() -> void:
	if not is_instance_valid(player):
		return
	if player.playing:
		return
	if stream.queued_packet_count() < startup_prebuffer_packets:
		return
	player.play()
	print("demo: playback started after prebuffer, queued_packets=", stream.queued_packet_count())


func _load_env_config() -> void:
	selected_input_device = OS.get_environment("GNA_DEMO_INPUT_DEVICE")
	selected_output_device = OS.get_environment("GNA_DEMO_OUTPUT_DEVICE")
	var fallback_env := OS.get_environment("GNA_DEMO_ALLOW_SYNTHETIC_FALLBACK").to_lower()
	if fallback_env in ["0", "false", "no"]:
		allow_synthetic_fallback = false
	var synthetic_env := OS.get_environment("GNA_DEMO_FORCE_SYNTHETIC").to_lower()
	if synthetic_env in ["1", "true", "yes"]:
		force_synthetic = true
	trace_jsonl_path = OS.get_environment("GNA_DEMO_TRACE_JSONL")
	var quit_env := OS.get_environment("GNA_DEMO_QUIT_SECONDS")
	if quit_env != "":
		var parsed := quit_env.to_float()
		if parsed > 0.0:
			quit_after_seconds = parsed
	var dtx_env := OS.get_environment("GNA_DEMO_ENABLE_DTX").to_lower()
	if dtx_env in ["0", "false", "no"]:
		enable_dtx = false
	var prebuffer_env := OS.get_environment("GNA_DEMO_STARTUP_PREBUFFER_PACKETS")
	if prebuffer_env != "":
		startup_prebuffer_packets = max(0, prebuffer_env.to_int())
