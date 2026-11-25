package vendor.oplus.hardware.oplusvibrator;

import vendor.aac.hardware.richtap.vibrator.IRichtapCallback;

interface IOplusVibrator {
    void init(@nullable IRichtapCallback callback);
    void setDynamicScale(int scale, @nullable IRichtapCallback callback);
    void setF0(int f0, @nullable IRichtapCallback callback);
    void stop(@nullable IRichtapCallback callback);
    void setAmplitude(int amplitude, @nullable IRichtapCallback callback);
    void performHeParam(int a, int b, int c, @nullable IRichtapCallback callback);
    void off(@nullable IRichtapCallback callback);
    void on(int amplitude, @nullable IRichtapCallback callback);
    int perform(int effectId, byte strength, @nullable IRichtapCallback callback);
    void performEnvelope(in int[] envInfo, boolean fastFlag, @nullable IRichtapCallback callback);
    void performRtp(in ParcelFileDescriptor fd, @nullable IRichtapCallback callback);
    void performHe(int a, int b, int c, int d, in int[] e, @nullable IRichtapCallback callback);
    void setHapticParam(in int[] params, int length, @nullable IRichtapCallback callback);
    void linearMotorVibratorOff();
    void linearMotorVibratorOn(int duration, int strength, boolean extra);
    void startCalibrate();
    int[] getCalibrateResults();
    int getStatus();
    void setVmax(int strength);
    void setMotorOldTest(int test);
    byte[] getSupportFeatures();
    void writeGunType(int type);
    void writeGunMode(int mode);
    void writeBulletNum(int num);
    void writeHapticAudio(String data);
    void setVibratorTouchStyle(int style);
    int getVibratorTouchStyle();
    void setPowerOnVibratorSwitch(String switchStr);
}
