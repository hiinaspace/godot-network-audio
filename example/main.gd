extends Node

const SAMPLE_RATE := 48_000
const FRAME_SAMPLES := 960
const FRAME_SECONDS := float(FRAME_SAMPLES) / float(SAMPLE_RATE)

var sender
var stream
var player: AudioStreamPlayer
var stats_timer: Timer
var quit_timer: Timer

var phase := 0.0
var total_generated_frames := 0
var send_accumulator := 0.0


func _ready() -> void:
	print("demo: ready")
	print("demo: class exists sender=", ClassDB.class_exists("NetworkAudioSender"))
	print("demo: class exists stream=", ClassDB.class_exists("AudioStreamNetwork"))

	sender = ClassDB.instantiate("NetworkAudioSender")
	stream = ClassDB.instantiate("AudioStreamNetwork")
	if sender == null or stream == null:
		push_error("demo: failed to instantiate extension classes")
		return

	sender.name = "Sender"
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
	quit_timer.wait_time = 4.0
	quit_timer.one_shot = true
	quit_timer.autostart = false
	quit_timer.timeout.connect(_shutdown_demo)
	add_child(quit_timer)

	player.play()
	stats_timer.start()
	quit_timer.start()

	print("demo: playback started")


func _process(delta: float) -> void:
	send_accumulator += delta
	while send_accumulator >= FRAME_SECONDS:
		send_accumulator -= FRAME_SECONDS
		_push_frame()


func _push_frame() -> void:
	var samples := PackedFloat32Array()
	samples.resize(FRAME_SAMPLES)

	for i in FRAME_SAMPLES:
		var t := phase + float(i) / float(SAMPLE_RATE)
		var carrier := sin(TAU * 220.0 * t)
		var overtone := 0.35 * sin(TAU * 440.0 * t)
		var wobble := 0.12 * sin(TAU * 3.0 * t)
		samples[i] = (carrier + overtone) * (0.18 + wobble)

	phase += FRAME_SECONDS
	total_generated_frames += 1
	var emitted: int = sender.push_pcm_mono(samples)
	if total_generated_frames <= 3:
		print("demo: pushed frame, emitted packets=", emitted)


func _on_packet_ready(bytes: PackedByteArray) -> void:
	var ok: bool = stream.push_packet(bytes)
	if not ok:
		push_error("demo: stream rejected packet bytes")


func _on_encoder_error(message: String) -> void:
	push_error("demo: encoder error: %s" % message)


func _print_stats() -> void:
	var stats: Dictionary = stream.get_stats()
	print("demo: stats ", JSON.stringify(stats))


func _shutdown_demo() -> void:
	print("demo: shutting down")
	if is_instance_valid(player):
		player.stop()
	if is_instance_valid(stats_timer):
		stats_timer.stop()
	get_tree().quit()
