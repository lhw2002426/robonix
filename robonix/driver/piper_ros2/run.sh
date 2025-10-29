source /opt/ros/humble/setup.bash
source install/setup.sh 
bash src/piper_ros/can_activate.sh can_piper 1000000 "1-4.1:1.0"
ros2 launch piper start_single_piper.launch.py can_port:=can_piper auto_enable:=True gripper_exist:=True gripper_val_mutiple:=2