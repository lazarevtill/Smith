/* Synthetic child-lifecycle fixture. No decoding, speech recognition or models. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/resource.h>
int main(int argc,char **argv){
 if(argc!=2)return 10;struct rlimit limit;
 if(getrlimit(RLIMIT_FSIZE,&limit)||limit.rlim_cur!=2097152||limit.rlim_max!=2097152)return 11;
 if(getrlimit(RLIMIT_CPU,&limit)||limit.rlim_cur!=20||limit.rlim_max!=20)return 12;
 FILE *p=fopen("decoder.pid","w");if(!p)return 13;fprintf(p,"%d",getpid());fclose(p);
 if(!strcmp(argv[1],"hold")){for(;;)usleep(10000);}
 if(!strcmp(argv[1],"flood")){for(int i=0;i<200000;i++)putchar('x');return 0;}
 if(!strcmp(argv[1],"oversize")){FILE*f=fopen("decoded.wav","w");if(!f)return 14;char bytes[4096]={0};for(int i=0;i<1024;i++)if(fwrite(bytes,1,sizeof bytes,f)!=sizeof bytes)return 15;fclose(f);return 16;}
 return strcmp(argv[1],"good")?17:0;
}
