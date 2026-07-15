fan
--------

## install

fan.service > /etc/systemd/system/fan.service

```sh
sudo systemctl daemon-reload
sudo systemctl enable fan
sudo systemctl start fan
```

## usage

```sh
fan -i 2 -p 60006 -n 200 -c 0:0,46:50,50:100,55:150,60:200,65:230,68:255
```

## dev

```
make build
make release
```

## 原理
基于rust实现一个app，支持传入两个参数: -i interval, -c config

读取cpu温度，大小核的温度取大值 / 1000 得到 摄氏度：

/sys/class/thermal/thermal_zone0/temp  表示cpul_thermal_zone 小核温度
/sys/class/thermal/thermal_zone1/temp  表示cpub_thermal_zone 大核温度

然后根据摄氏度来设置风扇的转速 0 - 255，一共映射7档
映射关系用启动参数传入（逗号间隔，摄氏度:转速）0:0,40:40,45:100,50:150
echo 0-255 > /sys/class/hwmon/hwmon9/pwm1



