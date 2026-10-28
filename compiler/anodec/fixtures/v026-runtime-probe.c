/* Representation evidence for the locally authored trusted value support. */
#include "runtime.h"
static size_t tracked_allocations=0;
static void* counted_alloc(size_t n) { ++tracked_allocations;return zn_alloc(n); }
#define zn_alloc counted_alloc
#include "bvalue.h"
#undef zn_alloc
int wmain(int argc,wchar_t** argv) {
    zn_args_init(argc,argv);
    B rows=b_soa(2);
    for(int64_t i=0;i<64;++i){B row=b_seq(BREC,2);row.o->data[0]=b_literal("owned");row.o->data[1]=b_int(i);b_push(&rows,&row);}
    zn_assert(rows.kind==BSOA && rows.o->len==2 && rows.o->count==64,1,"column descriptor");
    B* labels=&rows.o->data[0];B* scores=&rows.o->data[1];
    zn_assert(labels->kind==BSEQ && scores->kind==BSEQ && labels->o->len==64 && scores->o->len==64 && labels->o->data!=scores->o->data,2,"independent field columns");
    B snapshot=b_clone(&rows);B first=b_select(&rows,0);B replacement=b_literal("changed");b_assign(b_at(&first,0),&replacement);
    B previous=b_select(&snapshot,0);B expected=b_literal("owned");zn_assert(zn_text_eq(b_string(b_at(&previous,0)),b_string(&expected)),3,"independent clone");b_drop(&expected);
    B values=b_seq(BSEQ,0);for(int64_t i=0;i<16;++i){B value=b_int(i);b_push(&values,&value);}
    size_t before=tracked_allocations;int64_t owners=b_owners;B view=b_span(&values,4,8);B nested=b_span(&view,2,3);B empty=b_span(&nested,3,0);
    zn_assert(view.alias==values.o->data+4 && nested.alias==values.o->data+6 && nested.view_len==3 && empty.view_len==0,4,"zero-copy nested descriptors");
    b_swap(&nested,0,2);B number=b_int(90);b_replace(&nested,1,&number);
    zn_assert(tracked_allocations==before && b_owners==owners && b_integer(b_at(&values,6))==8 && b_integer(b_at(&values,7))==90 && b_integer(b_at(&values,8))==6,5,"span operations allocate nothing");
    b_drop(&empty);b_drop(&nested);b_drop(&view);b_drop(&values);b_drop(&snapshot);b_drop(&rows);b_clean();B message=b_literal("column storage and zero-allocation spans verified");b_print(&message);b_drop(&message);b_clean();return zn_process_exit_code();
}
