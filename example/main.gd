extends Node

const SAMPLE_RATE := 48_000
const FRAME_SAMPLES := 960
const FRAME_SECONDS := float(FRAME_SAMPLES) / float(SAMPLE_RATE)
const INPUT_MODE_MICROPHONE := "microphone"
const INPUT_MODE_SYNTHETIC := "synthetic"
const STARTUP_PREBUFFER_PACKETS := 6
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
var microphone_frame_budget := FRAME_SAMPLES
var synthetic_fallback_seconds := 0.75
var microphone_seen_frames := false
var demo_start_msec := 0
var prebuffer_remaining_packets := STARTUP_PREBUFFER_PACKETS
var selected_input_device := ""
var selected_output_device := ""
var allow_synthetic_fallback := true
var quit_after_seconds := DEFAULT_QUIT_SECONDS


func _ready() -> void:
	demo_start_msec = Time.get_ticks_msec()
	_load_env_config()
	print("demo: ready")
	print("demo: class exists sender=", ClassDB.class_exists("NetworkAudioSender"))
	print("demo: class exists stream=", ClassDB.class_exists("AudioStreamNetwork"))
	print("demo: current output device=", AudioServer.output_device)
	print("demo: output devices=", AudioServer.get_output_device_list())
	print("demo: current input device=", AudioServer.input_device)
	print("demo: input devices=", AudioServer.get_input_device_list())
	_select_output_device()
	_select_input_device()
	var input_active_result := AudioServer.set_input_device_active(true)
	print("demo: input active result=", input_active_result)

	sender = ClassDB.instantiate("NetworkAudioSender")
	stream = ClassDB.instantiate("AudioStreamNetwork")
	if sender == null or stream == null:
		push_error("demo: failed to instantiate extension classes")
		return

	sender.name = "Sender"
	sender.input_sample_rate_hz = int(AudioServer.get_mix_rate())
	add_child(sender)

	player = AudioStreamPlayer.new()
	player.name = "Player"
	player.stream = stream
	player.autoplay = false
	add_child(player)

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
		_pump_microphone()
		var elapsed_msec := Time.get_ticks_msec() - demo_start_msec
		if allow_synthetic_fallback and not microphone_seen_frames and elapsed_msec >= int(synthetic_fallback_seconds * 1000.0):
			input_mode = INPUT_MODE_SYNTHETIC
			print("demo: microphone fallback -> synthetic")
	else:
		send_accumulator += delta
		while send_accumulator >= FRAME_SECONDS:
			send_accumulator -= FRAME_SECONDS
			_push_synthetic_frame()


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


func _pump_microphone() -> void:
	var available: int = AudioServer.get_input_frames_available()
	if available <= 0:
		return

	microphone_seen_frames = true
	while available > 0:
		var chunk_frames: int = min(available, microphone_frame_budget)
		var stereo_frames: PackedVector2Array = AudioServer.get_input_frames(chunk_frames)
		if stereo_frames.is_empty():
			return

		var mono_samples := PackedFloat32Array()
		mono_samples.resize(stereo_frames.size())
		for i in stereo_frames.size():
			var frame: Vector2 = stereo_frames[i]
			mono_samples[i] = 0.5 * (frame.x + frame.y)
		_push_samples(mono_samples)
		available -= stereo_frames.size()


func _push_samples(samples: PackedFloat32Array) -> void:
	var emitted: int = sender.push_pcm_mono(samples)
	total_generated_frames += 1
	_maybe_start_playback()
	if total_generated_frames <= 3:
		print("demo: pushed samples=", samples.size(), " emitted packets=", emitted)


func _on_packet_ready(bytes: PackedByteArray) -> void:
	var ok: bool = stream.push_packet(bytes)
	if not ok:
		push_error("demo: stream rejected packet bytes")


func _on_encoder_error(message: String) -> void:
	push_error("demo: encoder error: %s" % message)


func _print_stats() -> void:
	var stats: Dictionary = stream.get_stats()
	print("demo: stats ", JSON.stringify(stats), " input_mode=", input_mode)


func _shutdown_demo() -> void:
	print("demo: shutting down")
	if is_instance_valid(player):
		player.stop()
	if is_instance_valid(stats_timer):
		stats_timer.stop()
	AudioServer.set_input_device_active(false)
	get_tree().quit()


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
	if prebuffer_remaining_packets > 0:
		prebuffer_remaining_packets -= 1
		if prebuffer_remaining_packets > 0:
			return
	player.play()
	print("demo: playback started after prebuffer, queued_packets=", stream.queued_packet_count())


func _load_env_config() -> void:
	selected_input_device = OS.get_environment("GNA_DEMO_INPUT_DEVICE")
	selected_output_device = OS.get_environment("GNA_DEMO_OUTPUT_DEVICE")
	var fallback_env := OS.get_environment("GNA_DEMO_ALLOW_SYNTHETIC_FALLBACK").to_lower()
	if fallback_env in ["0", "false", "no"]:
		allow_synthetic_fallback = false
	var quit_env := OS.get_environment("GNA_DEMO_QUIT_SECONDS")
	if quit_env != "":
		var parsed := quit_env.to_float()
		if parsed > 0.0:
			quit_after_seconds = parsed
