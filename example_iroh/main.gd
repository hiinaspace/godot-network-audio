extends Node

const SAMPLE_RATE := 48_000
const FRAME_SAMPLES := 960
const FRAME_SECONDS := float(FRAME_SAMPLES) / float(SAMPLE_RATE)
const ROLE_SENDER := "sender"
const ROLE_RECEIVER := "receiver"
const DEFAULT_QUIT_SECONDS := 6.0
const STARTUP_PREBUFFER_PACKETS := 4

var role := ROLE_RECEIVER
var quit_after_seconds := DEFAULT_QUIT_SECONDS
var max_fps := 0  # 0 = uncapped; set GNA_DEMO_MAX_FPS to override (e.g. 5 for receiver stress test)
var selected_input_device := ""
var selected_output_device := ""
var endpoint_info_path := ""
var remote_endpoint_info_path := ""
var trace_jsonl_path := ""
var allow_synthetic_fallback := true
var force_synthetic := false
var send_audio := true
var spatialize_receivers := false
var synthetic_frequency_hz := 220.0
var synthetic_fallback_seconds := 0.75
var startup_prebuffer_packets := STARTUP_PREBUFFER_PACKETS

var sender
var stream
var player
var receive_streams := {}
var receive_players := {}
var audio_listener: AudioListener3D
var transport
var stats_timer: Timer
var quit_timer: Timer
var trace_file: FileAccess = null
var trace_frame := 0
var input_mode := "microphone"
var demo_start_msec := 0
var phase := 0.0
var send_accumulator := 0.0
var peer_id := ""

func _ready() -> void:
	demo_start_msec = Time.get_ticks_msec()
	_load_env_config()
	# Cap frame rate very low so it is obvious in the trace whether audio
	# delivery still depends on _process(). Override with GNA_DEMO_MAX_FPS.
	Engine.max_fps = max_fps
	_open_trace_file()
	print("iroh demo: ready role=", role)
	print("iroh demo: current output device=", AudioServer.output_device)
	print("iroh demo: output devices=", AudioServer.get_output_device_list())
	print("iroh demo: current input device=", AudioServer.input_device)
	print("iroh demo: input devices=", AudioServer.get_input_device_list())
	_select_output_device()
	_select_input_device()

	transport = ClassDB.instantiate("IrohVoiceTransport")
	if transport == null:
		push_error("iroh demo: failed to instantiate classes")
		return

	add_child(transport)
	transport.peer_connected.connect(_on_peer_connected)
	transport.peer_replaced.connect(_on_peer_connected)
	if not transport.start_endpoint():
		push_error("iroh demo: failed to start endpoint")
		return

	_write_endpoint_info()

	if role == ROLE_RECEIVER:
		if spatialize_receivers:
			get_viewport().audio_listener_enable_3d = true
			audio_listener = AudioListener3D.new()
			add_child(audio_listener)
			audio_listener.make_current()
	else:
		if send_audio:
			sender = ClassDB.instantiate("NetworkAudioSender")
			if sender == null:
				push_error("iroh demo: failed to instantiate sender")
				return
			sender.input_sample_rate_hz = SAMPLE_RATE if force_synthetic else int(AudioServer.get_input_mix_rate())
			sender.capture_audio_server_input = not force_synthetic
			sender.microphone_frame_budget = FRAME_SAMPLES
			add_child(sender)
			# Wire sender directly to transport: encoded packets flow from the encode
			# thread → connection.send_datagram() without touching _process().
			transport.attach_sender(sender)
			if force_synthetic:
				input_mode = "synthetic"
		else:
			input_mode = "idle"

	stats_timer = Timer.new()
	stats_timer.wait_time = 1.0
	stats_timer.one_shot = false
	stats_timer.timeout.connect(_print_stats)
	add_child(stats_timer)
	stats_timer.start()

	quit_timer = Timer.new()
	quit_timer.wait_time = quit_after_seconds
	quit_timer.one_shot = true
	quit_timer.timeout.connect(_shutdown_demo)
	add_child(quit_timer)
	quit_timer.start()

	if role == ROLE_SENDER and remote_endpoint_info_path != "":
		_connect_remote_from_file()

func _process(delta: float) -> void:
	if role == ROLE_RECEIVER:
		_maybe_start_playback()
	else:
		if peer_id == "" and remote_endpoint_info_path != "":
			_connect_remote_from_file()
		if sender == null:
			pass
		elif input_mode == "microphone":
			var elapsed_msec := Time.get_ticks_msec() - demo_start_msec
			if allow_synthetic_fallback and sender.get_captured_input_frames() <= 0 and elapsed_msec >= int(synthetic_fallback_seconds * 1000.0):
				sender.capture_audio_server_input = false
				input_mode = "synthetic"
		else:
			send_accumulator += delta
			while send_accumulator >= FRAME_SECONDS:
				send_accumulator -= FRAME_SECONDS
				_push_synthetic_frame()
	_write_trace_row(delta)

func _connect_remote_from_file() -> void:
	if not FileAccess.file_exists(remote_endpoint_info_path):
		return
	var text := FileAccess.get_file_as_string(remote_endpoint_info_path)
	if text.strip_edges() == "":
		return
	var parsed = JSON.parse_string(text)
	if typeof(parsed) != TYPE_DICTIONARY:
		push_error("iroh demo: remote endpoint file is not a dictionary")
		remote_endpoint_info_path = ""
		return
	var info: Dictionary = parsed
	peer_id = String(info.get("endpoint_id", ""))
	if peer_id == "":
		push_error("iroh demo: remote endpoint info missing endpoint_id")
		remote_endpoint_info_path = ""
		return
	if not transport.connect_to_peer(info):
		push_error("iroh demo: failed to connect to peer")

func _maybe_start_playback() -> void:
	for connected_peer in receive_players:
		var peer_player = receive_players[connected_peer]
		var peer_stream = receive_streams[connected_peer]
		if not peer_player.playing and peer_stream.queued_packet_count() >= startup_prebuffer_packets:
			peer_player.play()

func _on_peer_connected(connected_peer_id: String) -> void:
	if role != ROLE_RECEIVER or receive_streams.has(connected_peer_id):
		return
	var peer_stream = transport.get_or_create_receive_stream(connected_peer_id)
	if peer_stream == null:
		push_error("iroh demo: failed to create receive stream for " + connected_peer_id)
		return
	var min_delay_str := OS.get_environment("GNA_DEMO_MIN_DELAY_MS")
	if min_delay_str != "":
		peer_stream.min_delay_ms = min_delay_str.to_int()
	var max_delay_str := OS.get_environment("GNA_DEMO_MAX_DELAY_MS")
	if max_delay_str != "":
		peer_stream.max_delay_ms = max_delay_str.to_int()
	var peer_player
	if spatialize_receivers:
		peer_player = AudioStreamPlayer3D.new()
		peer_player.attenuation_model = AudioStreamPlayer3D.ATTENUATION_DISABLED
		peer_player.max_distance = 100.0
		var index := receive_players.size()
		var angle := TAU * float(index) / 8.0
		peer_player.position = Vector3(cos(angle) * 2.0, 0.0, sin(angle) * 2.0)
	else:
		peer_player = AudioStreamPlayer.new()
	peer_player.stream = peer_stream
	add_child(peer_player)
	receive_streams[connected_peer_id] = peer_stream
	receive_players[connected_peer_id] = peer_player
	if stream == null:
		stream = peer_stream
		player = peer_player
	print("iroh demo: receive stream ready peer=", connected_peer_id, " count=", receive_streams.size())

func _push_synthetic_frame() -> void:
	var samples := PackedFloat32Array()
	samples.resize(FRAME_SAMPLES)
	for i in FRAME_SAMPLES:
		var t := phase + float(i) / float(SAMPLE_RATE)
		samples[i] = 0.18 * sin(TAU * synthetic_frequency_hz * t)
	phase += FRAME_SECONDS
	sender.push_pcm_mono(samples)

func _print_stats() -> void:
	var row := {
		"role": role,
		"transport": transport.get_stats(),
		"receiver": stream.get_stats() if stream != null else {},
		"sender": sender.get_stats() if sender != null else {},
		"peer_id": peer_id,
		"receive_stream_count": receive_streams.size(),
		"receivers": _receiver_stats(),
	}
	print("iroh demo: stats ", JSON.stringify(row))

func _shutdown_demo() -> void:
	_close_trace_file()
	if sender != null:
		pass  # direct send handler is owned by the closure, nothing to disconnect
	for connected_peer in receive_players:
		receive_players[connected_peer].stop()
		receive_players[connected_peer].stream = null
	receive_players.clear()
	receive_streams.clear()
	stream = null
	get_tree().quit()

func _open_trace_file() -> void:
	if trace_jsonl_path == "":
		return
	trace_file = FileAccess.open(trace_jsonl_path, FileAccess.WRITE)

func _close_trace_file() -> void:
	if trace_file != null:
		trace_file.flush()
		trace_file = null

func _write_trace_row(delta: float) -> void:
	if trace_file == null or transport == null:
		return
	var row := {
		"frame": trace_frame,
		"mono_usec": Time.get_ticks_usec(),
		"delta_sec": delta,
		"role": role,
		"peer_id": peer_id,
		"receive_stream_count": receive_streams.size(),
		"receivers": _receiver_stats(),
		"transport": transport.get_stats(),
		"receiver": stream.get_stats() if stream != null else {},
		"sender": sender.get_stats() if sender != null else {},
	}
	trace_file.store_line(JSON.stringify(row))
	trace_frame += 1

func _receiver_stats() -> Dictionary:
	var result := {}
	for connected_peer in receive_streams:
		result[connected_peer] = receive_streams[connected_peer].get_stats()
	return result

func _write_endpoint_info() -> void:
	if endpoint_info_path == "":
		return
	var info = transport.local_endpoint_info()
	var file := FileAccess.open(endpoint_info_path, FileAccess.WRITE)
	if file != null:
		file.store_string(JSON.stringify(info))
		file.flush()

func _select_input_device() -> void:
	var current := String(AudioServer.input_device)
	var devices := AudioServer.get_input_device_list()
	if selected_input_device != "" and devices.has(selected_input_device):
		AudioServer.input_device = selected_input_device
		print("iroh demo: selected input device=", selected_input_device)
		return
	if current != "" and devices.has(current):
		print("iroh demo: keeping input device=", current)
		return
	for device in devices:
		var name := String(device)
		if name != "" and name != "Default":
			AudioServer.input_device = name
			print("iroh demo: selected input device=", name)
			return
	if devices.has("Default"):
		AudioServer.input_device = "Default"
		print("iroh demo: selected input device=Default")
	else:
		print("iroh demo: no usable input device found")

func _select_output_device() -> void:
	var current := String(AudioServer.output_device)
	var devices := AudioServer.get_output_device_list()
	if selected_output_device != "" and devices.has(selected_output_device):
		AudioServer.output_device = selected_output_device
		print("iroh demo: selected output device=", selected_output_device)
		return
	if current != "" and devices.has(current):
		print("iroh demo: keeping output device=", current)
		return
	if devices.has("Default"):
		AudioServer.output_device = "Default"
		print("iroh demo: selected output device=Default")
	else:
		print("iroh demo: no usable output device found")

func _load_env_config() -> void:
	var value := OS.get_environment("GNA_IROH_ROLE")
	if value != "":
		role = value
	value = OS.get_environment("GNA_IROH_ENDPOINT_INFO_PATH")
	if value != "":
		endpoint_info_path = value
	value = OS.get_environment("GNA_IROH_REMOTE_INFO_PATH")
	if value != "":
		remote_endpoint_info_path = value
	value = OS.get_environment("GNA_DEMO_TRACE_JSONL")
	if value != "":
		trace_jsonl_path = value
	value = OS.get_environment("GNA_DEMO_INPUT_DEVICE")
	if value != "":
		selected_input_device = value
	value = OS.get_environment("GNA_DEMO_OUTPUT_DEVICE")
	if value != "":
		selected_output_device = value
	value = OS.get_environment("GNA_DEMO_QUIT_SECONDS")
	if value != "":
		quit_after_seconds = max(value.to_float(), 1.0)
	value = OS.get_environment("GNA_DEMO_ALLOW_SYNTHETIC_FALLBACK")
	if value != "":
		allow_synthetic_fallback = value != "0"
	value = OS.get_environment("GNA_DEMO_FORCE_SYNTHETIC")
	if value != "":
		force_synthetic = value != "0"
	value = OS.get_environment("GNA_DEMO_SEND_AUDIO")
	if value != "":
		send_audio = value != "0"
	value = OS.get_environment("GNA_DEMO_SPATIALIZE")
	if value != "":
		spatialize_receivers = value != "0"
	value = OS.get_environment("GNA_DEMO_SYNTHETIC_FREQUENCY_HZ")
	if value != "":
		synthetic_frequency_hz = max(value.to_float(), 20.0)
	value = OS.get_environment("GNA_DEMO_MAX_FPS")
	if value != "":
		max_fps = max(value.to_int(), 1)
