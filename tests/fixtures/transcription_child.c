/* Synthetic native protocol fixture. It does not recognize speech or load ML. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/stat.h>
int main(int argc,char **argv) {
  const char *expected[]={"-m","model.bin","-f","selected.wav","-l",NULL,"-np","-oj","-of","transcript","-ng","-t","2","-p","1"};
  if(argc!=16) return 10;
  for(int i=1;i<argc;i++) if(expected[i-1]&&strcmp(expected[i-1],argv[i])) return 11;
  if(strcmp(argv[6],"en")&&strcmp(argv[6],"ru")&&strcmp(argv[6],"auto")) return 12;
  struct stat st;if(stat(".",&st)|| (st.st_mode&0777)!=0700) return 13;
  if(stat("model.bin",&st)||(st.st_mode&0777)!=0400) return 14;
  if(getenv("HTTP_PROXY")||getenv("DYLD_LIBRARY_PATH")) return 15;
  FILE *wave=fopen("selected.wav","rb");if(!wave)return 16;fseek(wave,44,SEEK_SET);int mode=fgetc(wave);fclose(wave);
  FILE *pid=fopen("child.pid","w");if(!pid)return 17;fprintf(pid,"%d",getpid());fclose(pid);
  if(mode==1) {for(;;)usleep(10000);}
  if(mode==3) {for(int i=0;i<200000;i++) putchar('x');return 0;}
  if(mode==4) {symlink("model.bin","transcript.json");return 0;}
  if(mode==5) return 23;
  if(mode==6) {
    pid_t descendant=fork();if(descendant<0)return 19;
    if(descendant==0) {for(;;)usleep(10000);}
    FILE *record=fopen("descendant.pid","w");if(!record)return 20;
    fprintf(record,"%d",descendant);fclose(record);return 0;
  }
  FILE *out=fopen("transcript.json","w");if(!out)return 18;
  if(mode==2) {fputs("{\"transcription\":null}",out);fclose(out);return 0;}
  fputs("{\"result\":{\"language\":\"en\"},\"transcription\":[{\"offsets\":{\"from\":0,\"to\":500},\"text\":\" Hello & Привет. \"}]}",out);
  fclose(out);return 0;
}
