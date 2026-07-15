use std::collections::VecDeque;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const THERMAL_ZONE0: &str = "/sys/class/thermal/thermal_zone0/temp";
const THERMAL_ZONE1: &str = "/sys/class/thermal/thermal_zone1/temp";
const PWM1: &str = "/sys/class/hwmon/hwmon9/pwm1";

/// EMA 平滑系数默认值：smooth = alpha*temp + (1-alpha)*smooth。
/// 越小越平滑（抗抖强但响应略慢），常用 0.15~0.4；可通过 -a 参数覆盖。
/// 默认 0.2 偏向平滑，牺牲少量响应速度换取更稳的转速。
const DEFAULT_ALPHA: f64 = 0.2;

/// 软启动/调速限速：实际输出 PWM 每秒最大变化量。从静止启动时不会瞬间
/// 冲到高档，而是 actual 向 target 逐步逼近。越小越柔和（启动越慢）。
const RAMP_RATE_PER_SEC: f64 = 20.0;
/// 单次调节的最大步长上限，与采样间隔解耦，保证每一步都柔和不突跳。
const MAX_RAMP_STEP: u8 = 80;

struct FanState {
    history: VecDeque<f64>,
    current_pwm: u8,
    cpu: VecDeque<f64>,
    mem: VecDeque<f64>,
    disk: VecDeque<f64>,
}

fn read_temp(path: &str) -> f64 {
    let content = fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("Failed to read {}: {}", path, e);
        process::exit(1);
    });
    let millidegrees: f64 = content.trim().parse().unwrap_or_else(|e| {
        eprintln!("Failed to parse temperature from {}: {}", path, e);
        process::exit(1);
    });
    millidegrees / 1000.0
}

fn get_cpu_temp() -> f64 {
    let t0 = read_temp(THERMAL_ZONE0);
    let t1 = read_temp(THERMAL_ZONE1);
    t0.max(t1)
}

struct FanProfile {
    steps: Vec<(f64, u8)>,
}

impl FanProfile {
    fn parse(config: &str) -> Self {
        let mut steps: Vec<(f64, u8)> = config
            .split(',')
            .filter_map(|s| {
                let s = s.trim();
                if s.is_empty() {
                    return None;
                }
                let mut parts = s.split(':');
                let temp: f64 = parts.next()?.trim().parse().ok()?;
                let pwm: u8 = parts.next()?.trim().parse().ok()?;
                Some((temp, pwm))
            })
            .collect();

        if steps.is_empty() {
            eprintln!("Invalid config: no valid temperature:pwm pairs found");
            process::exit(1);
        }

        steps.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        FanProfile { steps }
    }

    fn get_pwm(&self, temp: f64) -> u8 {
        let mut pwm = self.steps[0].1;
        for &(t, p) in &self.steps {
            if temp >= t {
                pwm = p;
            } else {
                break;
            }
        }
        pwm
    }
}

fn set_fan_speed(pwm: u8) {
    if let Err(e) = fs::write(PWM1, pwm.to_string()) {
        eprintln!("Failed to write to {}: {}", PWM1, e);
    }
}

/// 读取 /proc/stat 的 aggregate CPU 时间，返回 (总时间, idle 时间)。
/// CPU 使用率需要两次采样求差得到，首次调用返回的值仅作为基准。
fn read_cpu_times() -> Option<(u64, u64)> {
    let s = fs::read_to_string("/proc/stat").ok()?;
    let first = s.lines().next()?;
    let mut it = first.split_whitespace();
    if it.next()? != "cpu" {
        return None;
    }
    let nums: Vec<u64> = it.filter_map(|x| x.parse::<u64>().ok()).collect();
    if nums.len() < 4 {
        return None;
    }
    // user nice system idle iowait ...：idle 为索引 3，iowait 为索引 4（若存在）。
    let idle = nums[3] + nums.get(4).copied().unwrap_or(0);
    let total: u64 = nums.iter().sum();
    Some((total, idle))
}

/// 内存使用率（%），基于 /proc/meminfo 的 MemAvailable（已扣除可回收缓存）。
fn mem_usage() -> Option<f64> {
    let s = fs::read_to_string("/proc/meminfo").ok()?;
    let mut total: Option<u64> = None;
    let mut avail: Option<u64> = None;
    for line in s.lines() {
        let mut it = line.split_whitespace();
        match it.next() {
            Some("MemTotal:") => total = it.next().and_then(|x| x.parse::<u64>().ok()),
            Some("MemAvailable:") => avail = it.next().and_then(|x| x.parse::<u64>().ok()),
            _ => {}
        }
    }
    let (total, avail) = (total?, avail?);
    if total == 0 {
        return None;
    }
    Some((total - avail) as f64 / total as f64 * 100.0)
}

/// 根分区磁盘使用率（%），解析 df 命令输出（保持零外部依赖）。
fn disk_usage() -> Option<f64> {
    let out = process::Command::new("df")
        .args(["-P", "/"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().nth(1)?;
    let pct = line.split_whitespace().nth(4)?;
    pct.trim_end_matches('%').parse::<f64>().ok()
}

/// 向定长缓冲追加一个采样，超出容量则丢弃最旧的一个（滚动窗口）。
fn push_capped(buf: &mut VecDeque<f64>, v: f64, cap: usize) {
    buf.push_back(v);
    if buf.len() > cap {
        buf.pop_front();
    }
}

const HTML_PAGE: &str = r#"<!DOCTYPE html>
<html><head><meta charset="utf-8">
<title>Fan Controller</title>
<style>
body{font-family:monospace;background:#1a1a2e;color:#e0e0e0;text-align:center;padding:20px}
h1{color:#00d4ff}
canvas{background:#16213e;border-radius:8px;margin:14px auto;display:block}
.stats{border-collapse:collapse;margin:14px auto}
.stats th,.stats td{border:1px solid #2a3a5c;padding:8px 26px;text-align:center}
.stats th{color:#00d4ff;font-size:14px}
.stats td{color:#e0e0e0;font-size:20px;min-width:78px}
.legend{color:#888;font-size:13px;margin:6px 0}
.info{color:#888;font-size:14px;margin-top:10px}
</style></head><body>
<h1>Fan Controller</h1>
<table class="stats">
<tr><th>温度</th><th>PWM</th><th>CPU</th><th>内存</th><th>磁盘</th></tr>
<tr><td id="temp">--</td><td id="pwm">--</td><td id="cpu">--</td><td id="mem">--</td><td id="disk">--</td></tr>
</table>
<div class="legend"><span style="color:#00d4ff">■</span> 温度 (°C，纵轴 20~70)</div>
<canvas id="chart" width="760" height="300"></canvas>
<div class="legend"><span style="color:#ffaa00">■</span> CPU &nbsp; <span style="color:#b388ff">■</span> 内存 &nbsp; <span style="color:#ff6688">■</span> 磁盘 (%)</div>
<canvas id="syschart" width="760" height="240"></canvas>
<div class="info">Auto refresh every <span id="interval"></span>s</div>
<script>
let temps=[],cpuH=[],memH=[],diskH=[];
let intervalSec=5;
function draw(){
  const c=document.getElementById('chart'),ctx=c.getContext('2d');
  const W=c.width,H=c.height,padL=70,padR=20,padT=20,padB=30;
  ctx.clearRect(0,0,W,H);
  if(temps.length<1)return;
  const minT=20,maxT=70,range=maxT-minT;
  // grid (5°C intervals)
  ctx.strokeStyle='#2a3a5c';ctx.lineWidth=1;
  ctx.font='12px monospace';ctx.textAlign='right';ctx.textBaseline='middle';
  for(let t=minT;t<=maxT;t+=5){
    const y=padT+(H-padT-padB)*(1-(t-minT)/range);
    ctx.beginPath();ctx.moveTo(padL,y);ctx.lineTo(W-padR,y);ctx.stroke();
    ctx.fillStyle='#668';
    ctx.fillText(t+'°C',padL-6,y);
  }
  // line
  ctx.strokeStyle='#00d4ff';ctx.lineWidth=2;ctx.beginPath();
  const stepX=W-padL-padR;
  temps.forEach((t,i)=>{
    const x=padL+(temps.length>1?stepX*i/(temps.length-1):stepX/2);
    const y=padT+(H-padT-padB)*(1-(t-minT)/range);
    i===0?ctx.moveTo(x,y):ctx.lineTo(x,y);
  });
  ctx.stroke();
  // dots
  ctx.fillStyle='#00ff88';
  temps.forEach((t,i)=>{
    const x=padL+(temps.length>1?stepX*i/(temps.length-1):stepX/2);
    const y=padT+(H-padT-padB)*(1-(t-minT)/range);
    ctx.beginPath();ctx.arc(x,y,3,0,Math.PI*2);ctx.fill();
  });
}
function lastVal(a){return a&&a.length?a[a.length-1].toFixed(1)+'%':'--';}
function drawSys(){
  const c=document.getElementById('syschart'),ctx=c.getContext('2d');
  const W=c.width,H=c.height,padL=50,padR=20,padT=20,padB=30;
  ctx.clearRect(0,0,W,H);
  // 网格 0~100%
  ctx.strokeStyle='#2a3a5c';ctx.lineWidth=1;
  ctx.font='12px monospace';ctx.textAlign='right';ctx.textBaseline='middle';
  for(let p=0;p<=100;p+=20){
    const y=padT+(H-padT-padB)*(1-p/100);
    ctx.beginPath();ctx.moveTo(padL,y);ctx.lineTo(W-padR,y);ctx.stroke();
    ctx.fillStyle='#668';
    ctx.fillText(p+'%',padL-6,y);
  }
  // CPU/内存/磁盘 三条百分比曲线
  const lines=[['#ffaa00',cpuH],['#b388ff',memH],['#ff6688',diskH]];
  lines.forEach(([col,arr])=>{
    if(arr.length<1)return;
    ctx.strokeStyle=col;ctx.lineWidth=2;ctx.beginPath();
    const stepX=W-padL-padR;
    arr.forEach((v,i)=>{
      const x=padL+(arr.length>1?stepX*i/(arr.length-1):stepX/2);
      const y=padT+(H-padT-padB)*(1-Math.max(0,Math.min(100,v))/100);
      i===0?ctx.moveTo(x,y):ctx.lineTo(x,y);
    });
    ctx.stroke();
  });
}
async function poll(){
  try{
    const r=await fetch('/api');
    const d=await r.json();
    temps=d.temps;cpuH=d.cpu;memH=d.mem;diskH=d.disk;
    document.getElementById('temp').textContent=temps.length?temps[temps.length-1].toFixed(1)+'°C':'--';
    document.getElementById('pwm').textContent=d.pwm;
    document.getElementById('cpu').textContent=lastVal(cpuH);
    document.getElementById('mem').textContent=lastVal(memH);
    document.getElementById('disk').textContent=lastVal(diskH);
    intervalSec=d.interval;
    document.getElementById('interval').textContent=intervalSec;
    draw();
    drawSys();
  }catch(e){console.error(e)}
}
poll();
setInterval(poll,5000);
</script></body></html>"#;

fn http_server(state: Arc<Mutex<FanState>>, port: u16, interval: u64) {
    let addr = format!("0.0.0.0:{}", port);
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Failed to bind {}: {}", addr, e);
            process::exit(1);
        }
    };
    println!("HTTP server listening on {}", addr);

    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        let mut buf = [0u8; 1024];
        let n = match stream.read(&mut buf) {
            Ok(0) | Err(_) => continue,
            Ok(n) => n,
        };
        let req = String::from_utf8_lossy(&buf[..n]);
        let path = req.lines().next().unwrap_or("").split_whitespace().nth(1).unwrap_or("/");

        let (content_type, body) = if path == "/api" {
            let s = state.lock().unwrap();
            let temps_json: String = s.history.iter().map(|t| format!("{:.1}", t)).collect::<Vec<_>>().join(",");
            let cpu_json: String = s.cpu.iter().map(|v| format!("{:.1}", v)).collect::<Vec<_>>().join(",");
            let mem_json: String = s.mem.iter().map(|v| format!("{:.1}", v)).collect::<Vec<_>>().join(",");
            let disk_json: String = s.disk.iter().map(|v| format!("{:.1}", v)).collect::<Vec<_>>().join(",");
            let body = format!(
                r#"{{"temps":[{}],"pwm":{},"interval":{},"cpu":[{}],"mem":[{}],"disk":[{}]}}"#,
                temps_json, s.current_pwm, interval,
                cpu_json, mem_json, disk_json
            );
            ("application/json", body)
        } else {
            ("text/html; charset=utf-8", HTML_PAGE.to_string())
        };

        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            content_type, body.len(), body
        );
        let _ = stream.write_all(resp.as_bytes());
    }
}

fn print_usage() -> ! {
    eprintln!("Usage: fan -i <interval_secs> -c <config> -n <history_size> -p <port> -a <ema_alpha>");
    eprintln!("Example: fan -i 5 -c 0:0,40:40,45:100,48:150,50:200,55:230,60:255 -n 10 -p 60006 -a 0.3");
    process::exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut interval: u64 = 5;
    let mut config = String::new();
    let mut history_size: usize = 10;
    let mut port: u16 = 60006;
    let mut alpha: f64 = DEFAULT_ALPHA;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-i" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("Missing value for -i");
                    print_usage();
                }
                interval = args[i].parse().unwrap_or_else(|e| {
                    eprintln!("Invalid interval: {}", e);
                    print_usage();
                });
            }
            "-c" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("Missing value for -c");
                    print_usage();
                }
                config = args[i].clone();
            }
            "-n" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("Missing value for -n");
                    print_usage();
                }
                history_size = args[i].parse().unwrap_or_else(|e| {
                    eprintln!("Invalid history size: {}", e);
                    print_usage();
                });
                if history_size == 0 {
                    eprintln!("History size must be >= 1");
                    process::exit(1);
                }
            }
            "-p" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("Missing value for -p");
                    print_usage();
                }
                port = args[i].parse().unwrap_or_else(|e| {
                    eprintln!("Invalid port: {}", e);
                    print_usage();
                });
            }
            "-a" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("Missing value for -a");
                    print_usage();
                }
                alpha = args[i].parse().unwrap_or_else(|e| {
                    eprintln!("Invalid alpha: {}", e);
                    print_usage();
                });
                if alpha <= 0.0 || alpha > 1.0 {
                    eprintln!("Alpha must be in (0, 1]");
                    print_usage();
                }
            }
            "-h" | "--help" => print_usage(),
            _ => {
                eprintln!("Unknown argument: {}", args[i]);
                print_usage();
            }
        }
        i += 1;
    }

    if config.is_empty() {
        eprintln!("Missing required argument: -c <config>");
        print_usage();
    }

    let profile = FanProfile::parse(&config);
    let state = Arc::new(Mutex::new(FanState {
        history: VecDeque::with_capacity(history_size),
        current_pwm: 0,
        cpu: VecDeque::with_capacity(history_size),
        mem: VecDeque::with_capacity(history_size),
        disk: VecDeque::with_capacity(history_size),
    }));

    {
        let state = Arc::clone(&state);
        thread::spawn(move || http_server(state, port, interval));
    }

    println!(
        "Fan controller started (interval={}s, alpha={}, {} steps, history={} samples, http port={})",
        interval,
        alpha,
        profile.steps.len(),
        history_size,
        port
    );

    let mut actual_pwm: u8 = 0; // 实际输出 PWM（软启动向 target 逐步逼近，从 0 起步）
    let mut smooth_temp: Option<f64> = None;
    let mut prev_cpu: Option<(u64, u64)> = None;

    // 启动时主动把硬件 PWM 同步到初始值 0。否则当目标档位恰好为 0（如温度低于
    // profile 最低档 40:0）时，循环里 actual 不会变化、永远不会写 PWM1，风扇就会
    // 沿用上次或固件默认的转速（可能狂转），而页面却显示 0——两者不一致。
    set_fan_speed(actual_pwm);

    loop {
        let temp = get_cpu_temp();

        // 系统指标：CPU 使用率需两次采样求差（首次为 None），内存、磁盘直接读取。
        let cpu = {
            let now = read_cpu_times();
            let usage = match (prev_cpu, now) {
                (Some((pt, pi)), Some((ct, ci))) => {
                    let dt = ct.saturating_sub(pt);
                    let di = ci.saturating_sub(pi);
                    if dt > 0 {
                        Some(((dt - di) as f64 / dt as f64) * 100.0)
                    } else {
                        None
                    }
                }
                _ => None,
            };
            prev_cpu = now;
            usage
        };
        let mem = mem_usage();
        let disk = disk_usage();

        // 更新状态：温度与系统指标各自维护滚动历史（网页绘图用真实温度）。
        {
            let mut s = state.lock().unwrap();
            push_capped(&mut s.history, temp, history_size);
            if let Some(c) = cpu {
                push_capped(&mut s.cpu, c, history_size);
            }
            if let Some(m) = mem {
                push_capped(&mut s.mem, m, history_size);
            }
            if let Some(d) = disk {
                push_capped(&mut s.disk, d, history_size);
            }
        }

        // 指数加权移动平均（EMA）：从源头滤掉瞬时抖动。首个采样直接取当前值。
        let smooth = match smooth_temp {
            None => temp,
            Some(prev) => alpha * temp + (1.0 - alpha) * prev,
        };
        smooth_temp = Some(smooth);

        // 目标转速由平滑温度查表得出。
        let target = profile.get_pwm(smooth);

        // 软启动：实际输出向 target 限速逼近，避免从静止瞬间冲到高档。
        // 步长 = 每秒变化率 × 采样间隔，并封顶 MAX_RAMP_STEP 保证每步柔和。
        let max_step = ((RAMP_RATE_PER_SEC * interval as f64).round() as u32)
            .min(MAX_RAMP_STEP as u32) as u8;
        let prev_actual = actual_pwm;
        actual_pwm = if target > actual_pwm {
            actual_pwm.saturating_add(max_step).min(target)
        } else {
            actual_pwm.saturating_sub(max_step).max(target)
        };

        if actual_pwm != prev_actual {
            set_fan_speed(actual_pwm);
            {
                let mut s = state.lock().unwrap();
                s.current_pwm = actual_pwm;
            }
            println!(
                "CPU temp: {:.1}C (smooth {:.1}C) -> PWM: {}->{} (target {})",
                temp, smooth, prev_actual, actual_pwm, target
            );
        } else {
            println!(
                "CPU temp: {:.1}C (smooth {:.1}C) -> PWM: {} (target {})",
                temp, smooth, actual_pwm, target
            );
        }

        thread::sleep(Duration::from_secs(interval));
    }
}
